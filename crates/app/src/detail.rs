//! Game detail view.
//!
//! Playtime, install state and profiles come from the scan and the database;
//! the About block comes from the cached store metadata. The Runtime panel adds
//! the per-game launch configuration: the Proton choice, the compiled shim
//! profile and the Steam launch options that route the game through it.

use std::path::PathBuf;

use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;

use pitagames_core::{AntiCheat, GameId, Profile, ProtonRef};
use pitagames_db::{Db, HistoryRepository, StoredLaunch, StoredProfile};
use pitagames_platform::proton::{DiscoveryReport, ProtonInstall};
use pitagames_stores::compat_tool::CompatTool;

use crate::View;
use crate::anticheat::AnticheatView;
use crate::copy::CopyButton;
use crate::diagnostics::DiagnosticsEditor;
use crate::focus::{self, ActionChannel};
use crate::format;
use crate::library::GameEntry;
use crate::persist;
use crate::play::{Play, PlayEvent};
use crate::runtime::{self, DiagnosticsDraft, WrapperDraft};
use crate::wrappers::WrapperEditor;
use pitagames_input::Action;

/// Sessions the detail page's history card shows.
const HISTORY_ROWS: usize = 5;

/// `Db` wraps a SQLite pool and is not `PartialEq`; props only need to know
/// whether a handle exists, which is what this wrapper compares.
#[derive(Clone)]
pub struct DbHandle(pub Option<Db>);

impl PartialEq for DbHandle {
    fn eq(&self, other: &Self) -> bool {
        self.0.is_some() == other.0.is_some()
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct DetailProps {
    pub entry: GameEntry,
    pub db: DbHandle,
    /// Back button target; navigation stays in `main.rs`.
    pub view: Signal<View>,
    /// The screen that opened this page. Back returns here rather than always
    /// to the library, so a game opened from the console home comes back to the
    /// home shelves.
    pub back: Signal<View>,
    /// The app's one input pump posts this screen's actions here.
    pub actions: ActionChannel,
    /// True while a text field owns the keyboard; the profile name input sets it.
    pub text_focused: Signal<bool>,
    /// Loopback URL for the header art, once it is on disk.
    pub hero: Option<String>,
    /// The Steam root the game came from; `None` disables the routing panel.
    pub steam_root: Option<PathBuf>,
    /// Proton builds discovered on this machine.
    pub proton: Option<DiscoveryReport>,
    /// The one running non-Steam game, so this page's Play button becomes a
    /// state label while it runs.
    pub play: Signal<Play>,
    pub play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
    /// Bumped when a supervised session ends, so the history card reloads.
    pub history_revision: Signal<u64>,
}

/// The detail screen for one game.
#[allow(non_snake_case)]
pub fn Detail(props: DetailProps) -> Element {
    let entry = props.entry;
    let db = props.db.0;
    let mut view = props.view;
    let back = props.back;
    let actions = props.actions;
    let mut text_focused = props.text_focused;
    let screen_focus = focus::use_focus();
    let game_id = entry.game.id.clone();

    let rows = use_signal(Vec::<StoredProfile>::new);
    let mut selected = use_signal(|| None::<String>);
    let mut new_name = use_signal(String::new);
    let mut error = use_signal(String::new);

    // Runtime panel: the profile's Proton choice and wrapper chain, the routing
    // state and the last action's message.
    let mut proton_choice = use_signal(|| ProtonRef::Auto);
    let mut wrapper_draft = use_signal(WrapperDraft::default);
    let mut diagnostics_draft = use_signal(DiagnosticsDraft::default);
    let mut runtime_message = use_signal(String::new);
    let mut routing = use_signal(|| None::<runtime::RoutingStatus>);
    // The compatibility tool Steam is (or is not) forced to use for this game.
    let mut compat = use_signal(|| None::<CompatTool>);
    // The detected/curated anti-cheat summary and its injection warning.
    let mut anticheat = use_signal(AnticheatView::pending);

    // Load the profiles once per mounted game; the parent keys this component
    // by appid so opening another game remounts it.
    let db_future = db.clone();
    let game_future = game_id.clone();
    use_future(move || {
        let db = db_future.clone();
        let game_id = game_future.clone();
        async move {
            load_profiles(
                db,
                game_id,
                rows,
                selected,
                proton_choice,
                wrapper_draft,
                diagnostics_draft,
                error,
            )
            .await;
        }
    });

    // Read the current launch options once; the Apply buttons refresh this.
    let root_future = props.steam_root.clone();
    let app_future = entry.app_id().to_string();
    use_future(move || {
        let root = root_future.clone();
        let app_id = app_future.clone();
        async move {
            if let Some(root) = root.as_deref() {
                routing.set(Some(runtime::routing_status(root, &app_id)));
                if let Ok(tool) = runtime::read_compat(root, &app_id) {
                    compat.set(tool);
                }
            }
        }
    });

    // Anti-cheat: walk the install directory and merge the compat DB's curated
    // value. Read-only, and cheap enough to run once per game open.
    let ac_dir = entry.game.install_dir.clone();
    let ac_game = game_id.clone();
    use_future(move || {
        let dir = ac_dir.clone();
        let id = ac_game.clone();
        async move {
            let db = pitagames_platform::compat::CompatDb::load();
            let curated = db
                .get(&id)
                .map(|entry| entry.anticheat)
                .unwrap_or(AntiCheat::Unknown);
            anticheat.set(crate::anticheat::summarize(dir.as_deref(), curated));
        }
    });

    // The sessions PitaGames started for this game. Reloaded on mount, and
    // whenever a session ends: reading the revision subscribes this future to
    // it, so a game played from this page fills its card in without a reload.
    let history_rows = use_signal(Vec::<StoredLaunch>::new);
    let history_revision = props.history_revision;
    let history_db = db.clone();
    let history_game = game_id.clone();
    use_future(move || {
        let db = history_db.clone();
        let id = history_game.clone();
        async move {
            let _ = history_revision();
            load_history(db, id, history_rows).await;
        }
    });

    let title = entry.title().to_string();
    let status = entry.status_label();
    let playtime = format::playtime(entry.playtime());
    let now = time::OffsetDateTime::now_utc();
    let last_played = entry
        .game
        .last_played
        .map(|at| format::last_played(at, now))
        .unwrap_or_else(|| "never".to_string());
    let size = entry
        .size_bytes()
        .map(format::human_bytes)
        .unwrap_or_else(|| "unknown".to_string());
    let install_path = entry
        .game
        .install_dir
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let version = entry
        .game
        .version
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    let anticheat_view = anticheat();
    let anticheat_label = anticheat_view.label.clone();
    let anticheat_warning = anticheat_view.warning.clone();
    let anticheat_evidence = anticheat_view.evidence.clone();

    // Store metadata is a cache: the page renders fine without it, and the
    // grid's background refresh fills this in without a reload.
    let store_metadata = entry.store_metadata().cloned();
    let metadata_age = entry
        .metadata_fetched_at()
        .map(|fetched_at| format::last_played(fetched_at, now));
    let app_kind = store_metadata
        .as_ref()
        .map(|metadata| metadata.kind.as_str().to_string())
        .filter(|kind| kind != "game");

    let about_body = match store_metadata {
        Some(metadata) => {
            let description = metadata.short_description.clone();
            let genres = metadata.genres.clone();
            let mut facts: Vec<(&'static str, String)> = Vec::new();
            if !metadata.developers.is_empty() {
                facts.push(("Developers", metadata.developers.join(", ")));
            }
            if !metadata.publishers.is_empty() {
                facts.push(("Publishers", metadata.publishers.join(", ")));
            }
            if let Some(date) = &metadata.release_date {
                let value = if metadata.coming_soon {
                    format!("coming soon · {date}")
                } else {
                    date.clone()
                };
                facts.push(("Released", value));
            }
            if let Some(score) = metadata.metacritic {
                facts.push(("Metacritic", format!("{score} / 100")));
            }
            facts.push(("Platforms", platforms_label(metadata.platforms)));
            if metadata.is_free {
                facts.push(("Price", "free to play".to_string()));
            }

            rsx! {
                if let Some(description) = description {
                    p {
                        style: "margin:0;font-size:13px;line-height:1.5;color:#c8c8d4;max-width:80ch;",
                        "{description}"
                    }
                }

                if !genres.is_empty() {
                    div {
                        style: "display:flex;flex-wrap:wrap;gap:6px;",
                        for genre in genres {
                            span {
                                key: "{genre}",
                                style: "padding:3px 10px;border-radius:999px;border:1px solid #2c2c38;background:#1e1e28;color:#a8a8b8;font-size:11px;",
                                "{genre}"
                            }
                        }
                    }
                }

                div {
                    style: "display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px;",
                    for (label, value) in facts {
                        {info_card(label, value)}
                    }
                }
            }
        }
        None => {
            let message = if entry.is_steam() {
                "No store metadata cached yet. Steam is asked in the background after the library loads."
            } else {
                "No description yet: this store's library listing carries only titles."
            };
            rsx! {
                p {
                    style: "margin:0;font-size:12px;color:#78788a;",
                    "{message}"
                }
            }
        }
    };

    let profile_rows = rows.read().clone();
    let has_profiles = !profile_rows.is_empty();
    let selected_name = selected.read().clone();
    let name = new_name.read().clone();
    let error_text = error.read().clone();
    let can_play = entry.can_play();
    // Steam launches through its own client; the other stores go through the
    // umu/Proton hand-off, which needs the store listing's executable.
    let play_wired = entry.play_is_wired();
    let play_blocked = entry.play_blocked();
    let play_signal = props.play;
    let play_events = props.play_events;
    let playing = play_signal.read().label(&game_id);
    let is_steam = entry.is_steam();

    // The history card's rows: one line per session, newest first, with the
    // transcript path so a failure can be read without leaving the page.
    let sessions: Vec<LaunchRow> = history_rows
        .read()
        .iter()
        .map(|stored| {
            let record = &stored.record;
            LaunchRow {
                key: stored.id,
                when: format::log_time(record.started_at, now),
                mode: record
                    .mode
                    .clone()
                    .unwrap_or_else(|| "not started".to_string()),
                verdict: record
                    .detail
                    .clone()
                    .unwrap_or_else(|| "no verdict recorded".to_string()),
                ok: record.succeeded,
                log: record
                    .log_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
            }
        })
        .collect();

    // The selected profile, resolved from the live rows so the Runtime panel
    // edits exactly the profile the "active" badge marks.
    let selected_profile = selected_name
        .as_deref()
        .and_then(|name| profile_rows.iter().find(|row| row.profile.name == name))
        .map(|row| row.profile.clone());

    let proton_options = proton_choices(props.proton.as_ref());
    let current_choice = proton_choice();
    // The Steam compatibility-tool name for the selected choice, when it can be
    // forced without guessing (compatibilitytools.d builds only).
    let current_tool = proton_options
        .iter()
        .find(|option| option.reference == current_choice)
        .and_then(|option| option.tool_name.clone());
    let routing_status = routing.read().clone();
    let compat_status = compat.read().clone();
    let runtime_note = runtime_message.read().clone();
    let compat_label = match &compat_status {
        Some(tool) => format!("Steam forces {} for this game.", tool.name),
        None => "Steam compatibility tool: (Steam decides).".to_string(),
    };
    let steam_root = props.steam_root.clone();

    // The resolved command mirrors what will actually run: `pitagames-shim` for
    // a Steam game, the umu/Proton (or store-client) hand-off for every other
    // store. Both recompute on every render from the selected profile plus the
    // live drafts, so toggling a diagnostic or a wrapper shows up immediately.
    let preview_app = entry.app_id().to_string();
    let draft_preview: Result<Profile, String> = match selected_profile.as_ref() {
        Some(profile) => {
            let mut preview = profile.clone();
            preview.proton = current_choice.clone();
            // Apply the two drafts in order; a refused wrapper draft stops
            // before the diagnostics are touched.
            let applied = wrapper_draft().apply_to(&mut preview);
            match applied {
                Ok(()) => diagnostics_draft().apply_to(&mut preview).map(|()| preview),
                Err(message) => Err(message),
            }
        }
        None => Err("Create and select a profile to see the launch command.".to_string()),
    };

    let mut handoff_note: Option<String> = None;
    let command_preview: Result<String, String> = match (draft_preview, props.proton.as_ref()) {
        (Ok(preview), Some(report)) if is_steam => {
            runtime::resolved_preview(&preview_app, &preview, report)
        }
        (Ok(preview), Some(report)) => {
            runtime::handoff_preview(&entry.game, &preview, report).map(|handoff| {
                handoff_note = Some(match &handoff.prefix {
                    Some(prefix) => format!("{} · prefix {prefix}", handoff.mode),
                    None => format!("{} · no per-game prefix", handoff.mode),
                });
                handoff.command
            })
        }
        (Ok(_), None) => Err("Proton discovery is not ready yet.".to_string()),
        (Err(message), _) => Err(message),
    };
    let (command_text, command_error) = match command_preview {
        Ok(command) => (command, None),
        Err(message) => (String::new(), Some(message)),
    };

    let play_entry = entry.clone();
    let play_db = db.clone();
    let create_db = db.clone();
    let create_game = game_id.clone();

    // Runtime panel handler captures.
    let save_db = db.clone();
    let save_report = props.proton.clone();
    let save_target = selected_profile.clone();
    let save_game = game_id.clone();
    let route_root = steam_root.clone();
    let route_app = entry.app_id().to_string();
    // Computed before `save_report` is moved into the button's closure.
    let save_available = selected_profile.is_some() && save_report.is_some();

    // The detail screen's action drain. The app's one pump posts here; this
    // screen owns its own graph. The cells are the two primary actions the page
    // always offers: Back, then Play when the game is wired and not running.
    // The runtime panels keep their pointer controls; a controller can reach
    // Back and Play without a pointer.
    let action_entry = play_entry.clone();
    let action_db = play_db.clone();
    let action_game = game_id.clone();
    use_future(move || {
        let channel = actions.clone();
        let entry = action_entry.clone();
        let db = action_db.clone();
        let game = action_game.clone();
        async move {
            while let Some(action) = channel.next().await {
                if view() != View::Detail {
                    continue;
                }
                let playing = play_signal.read().label(&game).is_some();
                let count = if play_wired && !playing { 2 } else { 1 };
                match action {
                    Action::Back => view.set(back()),
                    action if action.is_navigation() => {
                        screen_focus.move_focus(action, count, 1);
                    }
                    Action::Confirm => match screen_focus.index() {
                        Some(0) => view.set(back()),
                        Some(1) if play_wired && !playing => {
                            start_play(error, play_signal, play_events, entry.clone(), db.clone());
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    });

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:18px;padding:0 32px 40px;",

            {
                let focused = screen_focus.index() == Some(0);
                let ring = focus::ring_style(focused);
                let label = back_label(back());
                rsx! {
                    button {
                        onclick: move |_| view.set(back()),
                        style: "align-self:flex-start;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:12px;cursor:pointer;{ring}",
                        "{label}"
                    }
                }
            }

            if let Some(hero) = props.hero {
                div {
                    style: "width:100%;max-width:760px;aspect-ratio:460/215;border-radius:12px;overflow:hidden;background:#1e1e28;",
                    img {
                        src: "{hero}",
                        alt: "",
                        style: "width:100%;height:100%;object-fit:cover;display:block;",
                    }
                }
            }

            div {
                style: "display:flex;align-items:baseline;gap:14px;flex-wrap:wrap;",
                h2 {
                    style: "margin:0;font-size:24px;font-weight:600;color:#ffffff;",
                    "{title}"
                }
                if let Some(kind) = app_kind.clone() {
                    span {
                        style: "padding:2px 8px;border-radius:999px;border:1px solid #3a3a4c;color:#a8a8b8;font-size:10px;text-transform:uppercase;letter-spacing:0.06em;",
                        "{kind}"
                    }
                }
                if let Some(status) = status {
                    span {
                        style: "font-size:10px;text-transform:uppercase;letter-spacing:0.06em;color:#d8b070;",
                        "{status}"
                    }
                }
                if let Some(label) = playing {
                    span {
                        style: "padding:8px 12px;border-radius:8px;border:1px solid #3a5a48;background:#243a2e;color:#b8dcc4;font-size:12px;",
                        "{label}"
                    }
                } else if play_wired {
                    {
                        let focused = screen_focus.index() == Some(1);
                        let ring = focus::ring_style(focused);
                        rsx! {
                            button {
                                disabled: !can_play,
                                onclick: move |_| {
                                    start_play(
                                        error,
                                        play_signal,
                                        play_events,
                                        play_entry.clone(),
                                        play_db.clone(),
                                    );
                                },
                                style: "{play_style(can_play)}{ring}",
                                "Play"
                            }
                        }
                    }
                } else if let Some(reason) = play_blocked {
                    // A button that cannot launch is worse than a sentence that
                    // says so; the grid's Install action is the way forward here.
                    span {
                        style: "padding:8px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1a1a22;color:#78788a;font-size:12px;",
                        "{reason}"
                    }
                }
            }

            if !error_text.is_empty() {
                div {
                    style: "padding:10px 14px;border-radius:8px;background:#3a1e1e;color:#e8a0a0;font-size:12px;",
                    "{error_text}"
                }
            }

            div {
                style: "display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:12px;",
                {info_card("Playtime", playtime)}
                {info_card("Last played", last_played)}
                {info_card("Size", size)}
                {info_card("Install path", install_path)}
                {info_card("Version", version)}
                {info_card("Anti-cheat", anticheat_label)}
            }

            if let Some(warning) = anticheat_warning.clone() {
                div {
                    style: "display:flex;flex-direction:column;gap:4px;padding:10px 14px;border-radius:8px;background:#3a2e1e;color:#e8c890;font-size:12px;",
                    span {
                        style: "font-weight:600;",
                        "Anti-cheat injection warning"
                    }
                    span { "{warning}" }
                    for item in anticheat_evidence {
                        span {
                            style: "font-family:monospace;font-size:11px;color:#c8a870;overflow-wrap:anywhere;",
                            "{item}"
                        }
                    }
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:12px;padding-top:6px;",
                div {
                    style: "display:flex;align-items:baseline;gap:12px;",
                    h3 {
                        style: "margin:0;font-size:14px;font-weight:600;color:#ffffff;",
                        "About"
                    }
                    if let Some(age) = metadata_age.clone() {
                        span {
                            style: "font-size:11px;color:#78788a;",
                            "store data updated {age}"
                        }
                    }
                }
                {about_body}
            }

            div {
                style: "display:flex;flex-direction:column;gap:10px;padding-top:6px;",
                div {
                    style: "display:flex;align-items:baseline;gap:12px;",
                    h3 {
                        style: "margin:0;font-size:14px;font-weight:600;color:#ffffff;",
                        "Recent launches"
                    }
                    span {
                        style: "font-size:11px;color:#78788a;",
                        "the sessions PitaGames started"
                    }
                }
                if is_steam {
                    p {
                        style: "margin:0;font-size:12px;color:#78788a;",
                        "Steam starts this game itself, so its sessions belong to Steam. PitaGames records only the launches it supervises: the games it starts through umu, a native executable or a store client."
                    }
                } else if sessions.is_empty() {
                    p {
                        style: "margin:0;font-size:12px;color:#78788a;",
                        "No sessions yet. Press Play and the outcome lands here — including a launch that could not start, with the reason."
                    }
                } else {
                    for row in sessions {
                        div {
                            key: "{row.key}",
                            style: "display:flex;align-items:center;gap:12px;padding:10px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;",
                            span {
                                style: "font-size:12px;color:#a8a8b8;min-width:88px;",
                                "{row.when}"
                            }
                            span {
                                style: "font-family:monospace;font-size:11px;color:#7a9ad6;min-width:76px;",
                                "{row.mode}"
                            }
                            span {
                                style: if row.ok {
                                    "font-size:12px;color:#b8dcc4;flex:1;"
                                } else {
                                    "font-size:12px;color:#e8c890;flex:1;"
                                },
                                "{row.verdict}"
                            }
                            if let Some(log) = row.log.clone() {
                                span {
                                    style: "font-family:monospace;font-size:10px;color:#5a5a68;max-width:260px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                    "{log}"
                                }
                                CopyButton { text: log, label: "Copy log path".to_string() }
                            }
                        }
                    }
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:10px;padding-top:6px;",
                h3 {
                    style: "margin:0;font-size:14px;font-weight:600;color:#ffffff;",
                    "Profiles"
                }
                    p {
                        style: "margin:0;font-size:12px;color:#78788a;",
                        "A profile picks the Proton build and graphics settings this game launches with. New profiles start from the safe defaults."
                    }

                    if !has_profiles {
                        div {
                            style: "padding:16px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;color:#78788a;font-size:12px;",
                            "No profiles yet."
                        }
                    }

                    for row in profile_rows {
                        {
                            let profile_name = row.profile.name.clone();
                            let is_active = selected_name.as_deref() == Some(profile_name.as_str());
                            let pick_name = profile_name.clone();
                            let pick_proton = row.profile.proton.clone();
                            let pick_wrappers = row.profile.wrappers.clone();
                            let pick_diagnostics = row.profile.diagnostics;
                            let delete_name = profile_name.clone();
                            let delete_db = db.clone();
                            let delete_game = game_id.clone();

                            rsx! {
                                div {
                                    key: "{profile_name}",
                                    style: "display:flex;align-items:center;gap:12px;padding:12px 14px;border-radius:10px;background:#1e1e28;border:1px solid #2c2c38;",
                                    span {
                                        style: "font-size:13px;color:#e8e8ee;",
                                        "{profile_name}"
                                    }
                                    if is_active {
                                        span {
                                            style: "font-size:10px;text-transform:uppercase;letter-spacing:0.06em;color:#7a9ad6;",
                                            "active"
                                        }
                                    }
                                    button {
                                        onclick: move |_| {
                                            selected.set(Some(pick_name.clone()));
                                            proton_choice.set(pick_proton.clone());
                                            wrapper_draft.set(WrapperDraft::from_chain(&pick_wrappers));
                                            diagnostics_draft
                                                .set(DiagnosticsDraft::from_diagnostics(&pick_diagnostics));
                                        },
                                        style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:12px;cursor:pointer;",
                                        "Use"
                                    }
                                    button {
                                        onclick: move |_| {
                                            let db = delete_db.clone();
                                            let game_id = delete_game.clone();
                                            let name = delete_name.clone();
                                            spawn(async move {
                                                let Some(db) = db else {
                                                    error.set("Profiles are unavailable until the database opens.".to_string());
                                                    return;
                                                };
                                                match persist::delete_profile(&db, &game_id, &name).await {
                                                    Ok(_) => load_profiles(Some(db), game_id, rows, selected, proton_choice, wrapper_draft, diagnostics_draft, error).await,
                                                    Err(message) => error.set(message),
                                                }
                                            });
                                        },
                                        style: "padding:6px 12px;border-radius:8px;border:1px solid #4a2c2c;background:#2a1e1e;color:#e0a0a0;font-size:12px;cursor:pointer;",
                                        "Delete"
                                    }
                                }
                            }
                        }
                    }

                    div {
                        style: "display:flex;gap:8px;padding-top:4px;",
                        input {
                            placeholder: "New profile name",
                            value: "{name}",
                            oninput: move |event| new_name.set(event.value()),
                            onfocus: move |_| text_focused.set(true),
                            onblur: move |_| text_focused.set(false),
                            style: "flex:1;padding:8px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;outline:none;font-size:13px;",
                        }
                        button {
                            disabled: name.trim().is_empty(),
                            onclick: move |_| {
                                let name = new_name.read().trim().to_string();
                                if name.is_empty() {
                                    return;
                                }
                                let db = create_db.clone();
                                let game_id = create_game.clone();
                                spawn(async move {
                                    let Some(db) = db else {
                                        error.set("Profiles are unavailable until the database opens.".to_string());
                                        return;
                                    };
                                    match persist::save_profile(&db, &Profile::new(game_id.clone(), name)).await {
                                        Ok(_) => {
                                            new_name.set(String::new());
                                            load_profiles(Some(db), game_id, rows, selected, proton_choice, wrapper_draft, diagnostics_draft, error).await;
                                        }
                                        Err(message) => error.set(message),
                                    }
                                });
                            },
                            style: create_style(name.trim().is_empty()),
                            "Save profile"
                        }
                    }
                }

            div {
                style: "display:flex;flex-direction:column;gap:10px;padding-top:6px;",
                h3 {
                    style: "margin:0;font-size:14px;font-weight:600;color:#ffffff;",
                    "Runtime"
                }
                p {
                    style: "margin:0;font-size:12px;color:#78788a;",
                    if is_steam {
                        "PitaGames launches this game through pitagames-shim. Pick the Proton build and wrapper chain for the active profile, then route the game through the shim in Steam's launch options."
                    } else {
                        "Pick the Proton build and wrapper chain for the active profile. PitaGames starts this game itself — through umu with a per-game Wine prefix, or through the store's own client when it launches a game by id."
                    }
                }

                    if let Some(profile) = selected_profile.clone() {
                        div {
                            style: "font-size:11px;color:#78788a;",
                            "Editing the active profile "
                            span { style: "color:#a8a8b8;", "{profile.name}" }
                            "."
                        }
                    } else {
                        div {
                            style: "padding:12px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;color:#78788a;font-size:12px;",
                            "Create and select a profile to pick a Proton build."
                        }
                    }

                    div {
                        style: "display:flex;flex-direction:column;gap:6px;",
                        for (index, option) in proton_options.into_iter().enumerate() {
                            {
                                let reference = option.reference.clone();
                                let chosen = reference == current_choice;
                                let label = option.label.clone();
                                let detail = option.detail.clone();
                                rsx! {
                                    button {
                                        key: "{index}",
                                        onclick: move |_| proton_choice.set(reference.clone()),
                                        style: choice_style(chosen),
                                        span {
                                            style: "font-size:13px;font-weight:600;color:#e8e8ee;",
                                            "{label}"
                                        }
                                        span {
                                            style: "font-size:11px;color:#9a9aac;overflow-wrap:anywhere;",
                                            "{detail}"
                                        }
                                    }
                                }
                            }
                        }
                    }

                    WrapperEditor { draft: wrapper_draft }

                    DiagnosticsEditor { draft: diagnostics_draft }

                    div {
                        style: "display:flex;gap:8px;flex-wrap:wrap;",
                        button {
                            disabled: !save_available,
                            onclick: move |_| {
                                runtime_message.set(String::new());
                                let Some(profile) = save_target.clone() else {
                                    runtime_message.set("Create and select a profile first.".to_string());
                                    return;
                                };
                                let Some(report) = save_report.clone() else {
                                    runtime_message.set("Proton discovery is not ready yet.".to_string());
                                    return;
                                };
                                let Some(db) = save_db.clone() else {
                                    runtime_message.set("Profiles are unavailable until the database opens.".to_string());
                                    return;
                                };
                                let game = save_game.clone();
                                let choice = proton_choice();
                                let draft = wrapper_draft();
                                let diagnostics = diagnostics_draft();
                                spawn(async move {
                                    let mut profile = profile;
                                    profile.proton = choice;
                                    if let Err(message) = draft.apply_to(&mut profile) {
                                        runtime_message.set(message);
                                        return;
                                    }
                                    if let Err(message) = diagnostics.apply_to(&mut profile) {
                                        runtime_message.set(message);
                                        return;
                                    }
                                    if let Err(message) = persist::save_profile(&db, &profile).await {
                                        runtime_message.set(message);
                                        return;
                                    }
                                    match runtime::write_shim_profile(&profile, &report) {
                                        Ok((path, compiled)) => {
                                            runtime_message.set(format!(
                                                "Wrote {} · {} environment entr{}",
                                                path.display(),
                                                compiled.env.len(),
                                                if compiled.env.len() == 1 { "y" } else { "ies" }
                                            ));
                                            load_profiles(Some(db), game, rows, selected, proton_choice, wrapper_draft, diagnostics_draft, error).await;
                                        }
                                        Err(message) => runtime_message.set(message),
                                    }
                                });
                            },
                            style: action_style(save_available),
                            "Save & write profile"
                        }

                        if let Some(root) = route_root.clone().filter(|_| is_steam) {
                            button {
                                onclick: {
                                    let root = root.clone();
                                    let app_id = route_app.clone();
                                    move |_| {
                                        runtime_message.set(String::new());
                                        let root = root.clone();
                                        let app_id = app_id.clone();
                                        spawn(async move {
                                            match runtime::apply(&root, &app_id, false) {
                                                Ok(report) => {
                                                    runtime_message.set(format!(
                                                        "Routed through the shim: {}",
                                                        report.applied.unwrap_or_default()
                                                    ));
                                                    routing.set(Some(runtime::routing_status(&root, &app_id)));
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(true),
                                "Route through PitaGames"
                            }
                            button {
                                onclick: {
                                    let root = root.clone();
                                    let app_id = route_app.clone();
                                    move |_| {
                                        runtime_message.set(String::new());
                                        let root = root.clone();
                                        let app_id = app_id.clone();
                                        spawn(async move {
                                            match runtime::restore(&root, &app_id, false) {
                                                Ok(report) => {
                                                    runtime_message.set(format!(
                                                        "Restored original options: {}",
                                                        report.restored.unwrap_or_else(|| "(not set)".to_string())
                                                    ));
                                                    routing.set(Some(runtime::routing_status(&root, &app_id)));
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(true),
                                "Restore original"
                            }
                            button {
                                disabled: current_tool.is_none(),
                                onclick: {
                                    let root = root.clone();
                                    let app_id = route_app.clone();
                                    let tool_name = current_tool.clone();
                                    move |_| {
                                        runtime_message.set(String::new());
                                        let Some(name) = tool_name.clone() else {
                                            runtime_message.set(
                                                "Pick a build in a compatibilitytools.d folder first; Steam manages its own Proton builds.".to_string(),
                                            );
                                            return;
                                        };
                                        let root = root.clone();
                                        let app_id = app_id.clone();
                                        spawn(async move {
                                            let tool = CompatTool::new(name);
                                            match runtime::apply_compat(&root, &app_id, &tool, false) {
                                                Ok(report) => {
                                                    let applied = report
                                                        .applied
                                                        .map(|tool| tool.name)
                                                        .unwrap_or_default();
                                                    runtime_message.set(format!(
                                                        "Steam now forces {applied} for this game."
                                                    ));
                                                    compat.set(runtime::read_compat(&root, &app_id).ok().flatten());
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(current_tool.is_some()),
                                "Force in Steam"
                            }
                            button {
                                disabled: compat_status.is_none(),
                                onclick: {
                                    let root = root.clone();
                                    let app_id = route_app.clone();
                                    move |_| {
                                        runtime_message.set(String::new());
                                        let root = root.clone();
                                        let app_id = app_id.clone();
                                        spawn(async move {
                                            match runtime::clear_compat(&root, &app_id, false) {
                                                Ok(_) => {
                                                    runtime_message.set(
                                                        "Cleared the forced compatibility tool; Steam decides again.".to_string(),
                                                    );
                                                    compat.set(runtime::read_compat(&root, &app_id).ok().flatten());
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(compat_status.is_some()),
                                "Clear forced build"
                            }
                            button {
                                onclick: {
                                    let root = root.clone();
                                    let app_id = route_app.clone();
                                    move |_| {
                                        runtime_message.set(String::new());
                                        let root = root.clone();
                                        let app_id = app_id.clone();
                                        spawn(async move {
                                            match runtime::restore_compat(&root, &app_id, false) {
                                                Ok(report) => {
                                                    let restored = report
                                                        .restored
                                                        .map(|tool| tool.name)
                                                        .unwrap_or_else(|| "Steam decides".to_string());
                                                    runtime_message.set(format!(
                                                        "Restored the original compatibility tool: {restored}."
                                                    ));
                                                    compat.set(runtime::read_compat(&root, &app_id).ok().flatten());
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(true),
                                "Restore forced build"
                            }
                        }
                    }

                    if let Some(status) = routing_status.clone().filter(|_| is_steam) {
                        div {
                            style: "display:flex;flex-direction:column;gap:2px;font-size:11px;color:#78788a;",
                            span { "Steam: {status.steam.describe()}" }
                            if let Some(value) = status.launch_options.clone() {
                                span { "Launch options: {value}" }
                            } else {
                                span { "Launch options: (not set)" }
                            }
                            if status.shim_installed {
                                span { style: "color:#7a9ad6;", "Routed through pitagames-shim." }
                            }
                            span { "{compat_label}" }
                            if let Some(warning) = status.warning.clone() {
                                span { style: "color:#d8b070;", "{warning}" }
                            }
                        }
                    }

                    div {
                        style: "display:flex;flex-direction:column;gap:6px;padding-top:4px;",
                        div {
                            style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                            span {
                                style: "font-size:12px;font-weight:600;color:#e8e8ee;",
                                "Command"
                            }
                            CopyButton { text: command_text.clone() }
                            span {
                                style: "font-size:11px;color:#78788a;",
                                if is_steam {
                                    "what pitagames-shim will run (the same line as --explain)"
                                } else {
                                    "what the launch hand-off will run"
                                }
                            }
                        }
                        if let Some(message) = command_error.clone() {
                            span {
                                style: "padding:8px 12px;border-radius:8px;background:#3a2a1e;color:#e0c090;font-size:12px;overflow-wrap:anywhere;",
                                "{message}"
                            }
                        } else {
                            code {
                                style: "display:block;padding:10px 12px;border-radius:8px;background:#14141b;border:1px solid #26262f;color:#b8c8e8;font-family:monospace;font-size:11px;overflow-wrap:anywhere;white-space:pre-wrap;user-select:text;",
                                "{command_text}"
                            }
                        }
                        if let Some(note) = handoff_note.clone() {
                            span {
                                style: "font-size:11px;color:#78788a;",
                                "launch shape: {note}"
                            }
                        }
                    }

                    if !runtime_note.is_empty() {
                        div {
                            style: "padding:10px 14px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:12px;overflow-wrap:anywhere;",
                            "{runtime_note}"
                        }
                    }
                }
        }
    }
}

/// One row of the detail page's history card.
struct LaunchRow {
    /// The database row id, used as the rsx key.
    key: i64,
    when: String,
    mode: String,
    verdict: String,
    ok: bool,
    log: Option<String>,
}

/// Load the newest sessions for one game into the card.
///
/// A missing database or a read failure is a warning, not an error: the page
/// must render even when persistence is unavailable.
async fn load_history(db: Option<Db>, id: GameId, mut rows: Signal<Vec<StoredLaunch>>) {
    let Some(db) = db else {
        rows.set(Vec::new());
        return;
    };
    match db.launches_for(&id, HISTORY_ROWS).await {
        Ok(stored) => rows.set(stored),
        Err(error) => {
            eprintln!("[pitagames] could not read the launch history: {error}");
            rows.set(Vec::new());
        }
    }
}

/// Load (or reload) the profile list, keeping the current choice when it still
/// exists and otherwise selecting the first row.
#[allow(clippy::too_many_arguments)]
async fn load_profiles(
    db: Option<Db>,
    game_id: GameId,
    mut rows: Signal<Vec<StoredProfile>>,
    mut selected: Signal<Option<String>>,
    mut proton_choice: Signal<ProtonRef>,
    mut wrapper_draft: Signal<WrapperDraft>,
    mut diagnostics_draft: Signal<DiagnosticsDraft>,
    mut error: Signal<String>,
) {
    let Some(db) = db else {
        error.set("Profiles are unavailable until the database opens.".to_string());
        return;
    };

    match persist::profiles(&db, &game_id).await {
        Ok(profiles) => {
            let keep = selected.read().clone();
            let next = keep
                .filter(|name| profiles.iter().any(|row| &row.profile.name == name))
                .or_else(|| profiles.first().map(|row| row.profile.name.clone()));
            match next
                .as_deref()
                .and_then(|name| profiles.iter().find(|row| row.profile.name == name))
            {
                Some(row) => {
                    proton_choice.set(row.profile.proton.clone());
                    wrapper_draft.set(WrapperDraft::from_chain(&row.profile.wrappers));
                    diagnostics_draft
                        .set(DiagnosticsDraft::from_diagnostics(&row.profile.diagnostics));
                }
                None => {
                    wrapper_draft.set(WrapperDraft::default());
                    diagnostics_draft.set(DiagnosticsDraft::default());
                }
            }
            selected.set(next);
            rows.set(profiles);
            error.set(String::new());
        }
        Err(message) => error.set(message),
    }
}

fn info_card(label: &str, value: String) -> Element {
    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:4px;padding:12px 14px;border-radius:10px;background:#1e1e28;border:1px solid #2c2c38;",
            span {
                style: "font-size:10px;text-transform:uppercase;letter-spacing:0.06em;color:#78788a;",
                "{label}"
            }
            span {
                style: "font-size:13px;color:#e8e8ee;overflow-wrap:anywhere;",
                "{value}"
            }
        }
    }
}

fn platforms_label(platforms: pitagames_core::Platforms) -> String {
    let mut names = Vec::new();
    if platforms.windows {
        names.push("Windows");
    }
    if platforms.mac {
        names.push("macOS");
    }
    if platforms.linux {
        names.push("Linux");
    }
    if names.is_empty() {
        "none listed".to_string()
    } else {
        names.join(", ")
    }
}

/// The Back button's label for the screen it returns to.
///
/// A page opened from the console home says "Home"; one opened from the grid
/// says "Library". Any other origin keeps the grid's wording, since the grid is
/// the desktop shell's start screen.
fn back_label(back: View) -> &'static str {
    match back {
        View::Home => "← Home",
        _ => "← Library",
    }
}

/// Start the game from the detail page.
///
/// The Play button and the focus graph's `Confirm` both call this, so a
/// controller press and a pointer click cannot drift. The actual launch is
/// [`crate::start_play`], shared with the library grid and the home hero, so the
/// three surfaces always agree on how a Steam row and a store row start.
fn start_play(
    error: Signal<String>,
    play_signal: Signal<Play>,
    play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
    entry: GameEntry,
    db: Option<Db>,
) {
    crate::start_play(
        &entry.game,
        entry.playtime(),
        play_signal,
        play_events,
        error,
        db,
    );
}

fn play_style(enabled: bool) -> &'static str {
    if enabled {
        "margin-left:auto;padding:8px 22px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:13px;cursor:pointer;"
    } else {
        "margin-left:auto;padding:8px 22px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:13px;cursor:default;"
    }
}

fn create_style(disabled: bool) -> &'static str {
    if disabled {
        "padding:8px 16px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:13px;cursor:default;"
    } else {
        "padding:8px 16px;border-radius:8px;border:1px solid #3a3a4c;background:#243024;color:#d8e8d0;font-size:13px;cursor:pointer;"
    }
}

/// One selectable Proton runtime in the detail page.
struct ProtonChoice {
    reference: ProtonRef,
    label: String,
    detail: String,
    /// The Steam compatibility-tool name, when Steam can be forced to use this
    /// build (compatibilitytools.d entries only).
    tool_name: Option<String>,
}

/// Automatic, Steam default, then every build discovery found.
fn proton_choices(report: Option<&DiscoveryReport>) -> Vec<ProtonChoice> {
    let mut choices = vec![
        ProtonChoice {
            reference: ProtonRef::Auto,
            label: "Automatic".to_string(),
            detail: match report.and_then(|report| report.best()) {
                Some(best) => format!("best available: {} ({})", best.name, best.source.label()),
                None => "no Proton build found".to_string(),
            },
            tool_name: None,
        },
        ProtonChoice {
            reference: ProtonRef::SteamDefault,
            label: "Steam default".to_string(),
            detail: "Steam's own compatibility setting decides".to_string(),
            tool_name: None,
        },
    ];

    if let Some(report) = report {
        for install in &report.installs {
            choices.push(ProtonChoice {
                reference: ProtonRef::Path {
                    path: install.path.clone(),
                },
                label: install.name.clone(),
                detail: install_detail(install),
                tool_name: install.compat_tool_name().map(str::to_string),
            });
        }
    }

    choices
}

fn install_detail(install: &ProtonInstall) -> String {
    let mut parts = vec![
        install.kind.label().to_string(),
        install.source.label().to_string(),
    ];
    if let Some(version) = install.distinct_version() {
        parts.push(version.to_string());
    }
    parts.join(" · ")
}

fn choice_style(chosen: bool) -> &'static str {
    if chosen {
        "display:flex;flex-direction:column;gap:2px;align-items:flex-start;text-align:left;padding:10px 14px;border-radius:10px;border:1px solid #3f5c8c;background:#243044;cursor:pointer;"
    } else {
        "display:flex;flex-direction:column;gap:2px;align-items:flex-start;text-align:left;padding:10px 14px;border-radius:10px;border:1px solid #2c2c38;background:#1e1e28;cursor:pointer;"
    }
}

fn action_style(enabled: bool) -> String {
    if enabled {
        "padding:8px 16px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:13px;cursor:pointer;".to_string()
    } else {
        "padding:8px 16px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:13px;cursor:default;".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_back_label_names_the_screen_the_page_returns_to() {
        assert_eq!(back_label(View::Home), "← Home");
        assert_eq!(back_label(View::Library), "← Library");
        // Any other origin keeps the grid's wording; the grid is the desktop
        // shell's start screen, so that is the safe default.
        assert_eq!(back_label(View::Downloads), "← Library");
        assert_eq!(back_label(View::Detail), "← Library");
    }
}
