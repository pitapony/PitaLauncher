//! Game detail view.
//!
//! Playtime, install state and profiles come from the scan and the database;
//! the About block comes from the cached store metadata. The Runtime panel adds
//! the per-game launch configuration: the Proton choice, the compiled shim
//! profile and the Steam launch options that route the game through it.

use std::path::PathBuf;

use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;

use pitagames_core::i18n::t;
use pitagames_core::ui::{AppConfig, Shell};
use pitagames_core::{AntiCheat, GameId, Profile, ProtonRef};
use pitagames_db::{Db, HistoryRepository, StoredLaunch, StoredProfile};
use pitagames_platform::proton::{DiscoveryReport, ProtonInstall};
use pitagames_stores::compat_tool::CompatTool;

use crate::View;
use crate::anticheat::AnticheatView;
use crate::copy::CopyButton;
use crate::diagnostics::DiagnosticsEditor;
use crate::eac::EacView;
use crate::focus::{self, ActionChannel};
use crate::format;
use crate::game_page::{self, Badge, BadgeKind, BadgeTone, Primary};
use crate::library::GameEntry;
use crate::persist;
use crate::play::{Play, PlayEvent};
use crate::runtime::{self, DiagnosticsDraft, WrapperDraft};
use crate::shell;
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
    /// The app's one input pump posts this screen's actions here.
    pub actions: ActionChannel,
    /// Loopback URL for the header art, once it is on disk.
    pub hero: Option<String>,
    /// The Steam root the game came from; `None` disables the routing panel.
    pub steam_root: Option<PathBuf>,
    /// Proton builds discovered on this machine.
    pub proton: Option<DiscoveryReport>,
    /// Which shell the window is in: the console shell opens on a full-bleed
    /// hero, the desktop shell on the page's original chrome.
    pub shell: Signal<Shell>,
    /// Library folders a Steam install's tracker polls.
    pub libraries: Vec<PathBuf>,
}

/// The detail screen for one game.
#[allow(non_snake_case)]
pub fn Detail(props: DetailProps) -> Element {
    let entry = props.entry;
    let db = props.db.0;
    let actions = props.actions;
    // The session handle reaches every concern handle (ADR-0027): this page
    // reads the play slot, the install queue, the Downloads board and the
    // history revision through the one seam instead of eight props fields.
    let mut session = crate::context::use_session();
    let play_signal = session.read().play();
    let play_events = session.read().play_events();
    let installs = session.read().installs();
    let install_events = session.read().install_events();
    let download_events = session.read().download_events();
    let board = session.read().downloads();
    let history_revision = session.read().history_revision();
    let screen_focus = focus::use_focus();
    let game_id = entry.game.id.clone();

    let rows = use_signal(Vec::<StoredProfile>::new);
    let mut selected = use_signal(|| None::<String>);
    let mut new_name = use_signal(String::new);
    let mut error = use_signal(String::new);
    // The on-screen keyboard: opened by the pad's Search button while this page
    // shows, seeded from the name field, and submitted through the same
    // `create_profile` the pointer's Save button calls.
    let mut osk_open = use_signal(|| false);
    let mut osk = use_signal(pitagames_input::OnScreenKeyboard::new);

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
    // The EAC Linux v2 readiness verdict, and the consent-gated reboot action.
    let mut eac_view = use_signal(EacView::hidden);
    let mut reboot_confirm = use_signal(|| false);
    let mut eac_message = use_signal(String::new);
    // The game's curated compat entry, for the in-game hint card's note override.
    let mut compat_entry = use_signal(|| None::<pitagames_platform::compat::CompatEntry>);

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
            let entry = db.get(&id);
            compat_entry.set(entry.cloned());
            let curated = entry
                .map(|entry| entry.anticheat)
                .unwrap_or(AntiCheat::Unknown);
            let eac_v2 = entry.is_some_and(|entry| entry.eac_v2);
            let view = crate::anticheat::summarize(dir.as_deref(), curated);
            let effective = view.kind;
            anticheat.set(view);

            // The EAC Linux v2 verdict: the curated v2 claim folded with this
            // boot's readiness. Read-only, and it never runs `eac-setup`.
            let readiness =
                pitagames_platform::eac::probe(&pitagames_platform::eac::EacProbe::host());
            eac_view.set(crate::eac::summarize(effective, eac_v2, &readiness));
        }
    });

    // The sessions PitaGames started for this game. Reloaded on mount, and
    // whenever a session ends: reading the revision subscribes this future to
    // it, so a game played from this page fills its card in without a reload.
    let history_rows = use_signal(Vec::<StoredLaunch>::new);
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
        .unwrap_or_else(|| t("detail.never").to_string());
    let size = entry
        .size_bytes()
        .map(format::human_bytes)
        .unwrap_or_else(|| t("detail.unknown").to_string());
    let install_path = entry
        .game
        .install_dir
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| t("detail.unknown").to_string());
    let version = entry
        .game
        .version
        .clone()
        .unwrap_or_else(|| t("detail.unknown").to_string());

    let anticheat_view = anticheat();
    let anticheat_label = anticheat_view.label.clone();
    let anticheat_warning = anticheat_view.warning.clone();
    let anticheat_evidence = anticheat_view.evidence.clone();

    // The EAC Linux v2 card: shown only for a protected game.
    let eac = eac_view();
    let eac_show = eac.show;
    let eac_label = eac.label.clone();
    let eac_message_text = eac.message.clone();
    let eac_notice = eac.notice.clone();
    let eac_missing = eac.missing.clone();
    let eac_needs_entry = eac.needs_entry;
    let eac_command = eac.command_line.clone();
    let eac_reboot = eac.reboot.clone();
    let eac_report = eac_message.read().clone();
    let reboot_confirming = reboot_confirm();

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
                facts.push((t("detail.developers"), metadata.developers.join(", ")));
            }
            if !metadata.publishers.is_empty() {
                facts.push((t("detail.publishers"), metadata.publishers.join(", ")));
            }
            if let Some(date) = &metadata.release_date {
                let value = if metadata.coming_soon {
                    t("detail.coming_soon").replace("{date}", date)
                } else {
                    date.clone()
                };
                facts.push((t("detail.released"), value));
            }
            if let Some(score) = metadata.metacritic {
                facts.push((
                    t("detail.metacritic"),
                    t("detail.metacritic_score").replace("{score}", &score.to_string()),
                ));
            }
            facts.push((t("detail.platforms"), platforms_label(metadata.platforms)));
            if metadata.is_free {
                facts.push((t("detail.price"), t("detail.free_to_play").to_string()));
            }

            rsx! {
                if let Some(description) = description {
                    p {
                        style: "margin:0;font-size:var(--text-body);line-height:1.5;color:var(--text-secondary);max-width:80ch;",
                        "{description}"
                    }
                }

                if !genres.is_empty() {
                    div {
                        style: "display:flex;flex-wrap:wrap;gap:6px;",
                        for genre in genres {
                            span {
                                key: "{genre}",
                                style: "padding:3px 10px;border-radius:999px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-micro);",
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
                t("detail.no_metadata")
            } else {
                t("detail.no_description")
            };
            rsx! {
                p {
                    style: "margin:0;font-size:var(--text-caption);color:var(--text-muted);",
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
                    .unwrap_or_else(|| t("history.not_started").to_string()),
                verdict: record
                    .detail
                    .clone()
                    .unwrap_or_else(|| t("history.no_verdict").to_string()),
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

    // The in-game key hints for the selected profile's injection mode, with the
    // game's curated `[ingame] notes` as the note override. Pure and cheap, so
    // it recomputes with the profile; the shipped key data is parsed once.
    let hint_data = use_hook(pitagames_platform::hints::HintData::shipped);
    let in_game_hints = selected_profile.as_ref().and_then(|profile| {
        pitagames_platform::hints::set_for(
            &hint_data,
            profile,
            compat_entry
                .read()
                .as_ref()
                .and_then(|entry| entry.ingame.notes.as_deref()),
        )
    });

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
        Some(tool) => t("detail.steam_forces").replace("{tool}", &tool.name),
        None => t("detail.steam_compat_decides").to_string(),
    };
    let steam_root = props.steam_root.clone();

    // The console game page's model: only true badges, and the one primary
    // action. The desktop page keeps its original chrome, so the two shells read
    // the same facts without the desktop layout changing.
    let console = props.shell.read().is_console();
    let forced_tool = compat_status.as_ref().map(|tool| tool.name.clone());
    let badges = game_page::badges(&entry, anticheat_view.kind, forced_tool.as_deref());
    let primary = game_page::primary_action(&entry);
    let install_ready = (*install_events.read()).is_some();
    // The hero button and the hero badge read one source
    // (`GameEntry::install_status`), so they can never disagree.
    let install_label = game_page::primary_label(&entry, primary);
    let meta_line = entry.meta_line();
    let initial = title
        .chars()
        .next()
        .map(|first| first.to_uppercase().to_string())
        .unwrap_or_default();
    // The hero URL is needed by both shells' chrome; the desktop branch used to
    // move it out of `props`, so keep a clone for the console hero.
    let hero_url = props.hero.clone();
    // The console hero's Install action needs the same channels the grid uses.
    let action_installs = installs;
    let action_install_events = install_events;
    let action_events = download_events;
    let action_board = board;
    let action_libraries = props.libraries.clone();

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
        None => Err(t("detail.no_command_profile").to_string()),
    };

    let mut handoff_note: Option<String> = None;
    let command_preview: Result<String, String> = match (draft_preview, props.proton.as_ref()) {
        (Ok(preview), Some(report)) if is_steam => {
            runtime::resolved_preview(&preview_app, &preview, report)
        }
        (Ok(preview), Some(report)) => {
            runtime::handoff_preview(&entry.game, &preview, report).map(|handoff| {
                handoff_note = Some(match &handoff.prefix {
                    Some(prefix) => t("detail.handoff_prefix")
                        .replace("{mode}", handoff.mode)
                        .replace("{prefix}", prefix),
                    None => t("detail.handoff_no_prefix").replace("{mode}", handoff.mode),
                });
                handoff.command
            })
        }
        (Ok(_), None) => Err(t("detail.proton_not_ready").to_string()),
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
    // The sheet's submit path needs its own copies: `db` and `game_id` are
    // moved into other closures below.
    let osk_db = db.clone();
    let osk_game = game_id.clone();
    // The console hero's own button captures: the desktop branch below already
    // owns `play_entry`/`play_db`, so the hero needs its own copies.
    let hero_play_entry = entry.clone();
    let hero_play_db = db.clone();
    let hero_install_game = entry.game.clone();
    let hero_libraries = props.libraries.clone();
    // The Graphics screen's own captures (the desktop button and the pad's
    // `Context` action both open it).
    let graphics_game = game_id.clone();

    // The detail screen's action drain. The app's one pump posts here; this
    // screen owns its own graph. The cells are the two primary actions the page
    // always offers: Back, then Play or Install when the game can be started.
    // The runtime panels keep their pointer controls; a controller can reach
    // Back and the primary action without a pointer.
    let action_entry = play_entry.clone();
    let action_db = play_db.clone();
    let action_game = game_id.clone();
    use_future(move || {
        let channel = actions.clone();
        let entry = action_entry.clone();
        let db = action_db.clone();
        let game = action_game.clone();
        let libraries = action_libraries.clone();
        async move {
            while let Some(action) = channel.next().await {
                if session.read().view() != View::Detail {
                    continue;
                }
                // While the on-screen keyboard is open it owns the keys: a
                // direction walks its key grid, `Back` cancels the edit, and
                // `Done` creates the profile the name field would have made.
                if osk_open() {
                    let press = osk.write().handle(action);
                    match press {
                        pitagames_input::OskPress::Submit => {
                            let name = osk.read().text().to_string();
                            osk_open.set(false);
                            new_name.set(String::new());
                            spawn(create_profile(
                                db.clone(),
                                game.clone(),
                                name,
                                new_name,
                                rows,
                                selected,
                                proton_choice,
                                wrapper_draft,
                                diagnostics_draft,
                                error,
                            ));
                        }
                        pitagames_input::OskPress::Cancel => osk_open.set(false),
                        pitagames_input::OskPress::Idle | pitagames_input::OskPress::Edited => {}
                    }
                    continue;
                }
                // The pad's `Context` button (West) opens the graphics screen
                // for this game, in either shell — the console game page's route
                // to the M3 profile.
                if action == Action::Context {
                    session.write().open_graphics(game.clone());
                    continue;
                }
                // The pad's Search button is the page's text entry; the only
                // field here is the new profile's name.
                if action == Action::Search {
                    osk.set(pitagames_input::OnScreenKeyboard::with_text(
                        new_name.read().clone(),
                    ));
                    osk_open.set(true);
                    continue;
                }
                let playing = play_signal.read().label(&game).is_some();
                // The page offers one primary action under Back: Play when the
                // game is wired, Install when its files are missing or waiting.
                let has_primary = matches!(primary, Primary::Play | Primary::Install);
                let count = if has_primary && !playing { 2 } else { 1 };
                match action {
                    Action::Back => {
                        let to = session.read().back();
                        session.write().set_view(to);
                    }
                    action if action.is_navigation() => {
                        screen_focus.move_focus(action, count, 1);
                    }
                    Action::Confirm => match screen_focus.index() {
                        Some(0) => {
                            let to = session.read().back();
                            session.write().set_view(to);
                        }
                        Some(1) if !playing => match primary {
                            Primary::Play => {
                                start_play(
                                    error,
                                    play_signal,
                                    play_events,
                                    entry.clone(),
                                    db.clone(),
                                );
                            }
                            Primary::Install => crate::launch_start::start_install(
                                &entry.game,
                                action_installs,
                                action_install_events,
                                action_events,
                                action_board,
                                &libraries,
                                error,
                                session,
                            ),
                            Primary::Open => {}
                        },
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

            if console {
                // The console game page's full-bleed hero: the game's art behind
                // its true badges, its title and its one primary action. Back is
                // overlaid top-left (the cheat-sheet footer also names it), and
                // the existing panels carry on below the fold.
                div {
                    style: "position:relative;margin:0 -32px;height:44vh;min-height:280px;overflow:hidden;background:#1e1e28;",

                    if let Some(url) = hero_url.clone() {
                        img {
                            src: "{url}",
                            alt: "",
                            style: "position:absolute;inset:0;width:100%;height:100%;object-fit:cover;display:block;",
                        }
                    } else {
                        div {
                            style: "position:absolute;inset:0;display:flex;align-items:center;justify-content:center;background:linear-gradient(135deg,#26263a,#191922);color:#414158;font-size:72px;font-weight:600;",
                            "{initial}"
                        }
                    }

                    div {
                        style: "position:absolute;inset:0;background:linear-gradient(90deg, rgba(12,12,18,0.94) 0%, rgba(12,12,18,0.55) 55%, rgba(12,12,18,0.10) 100%);",
                    }

                    {
                        let focused = screen_focus.index() == Some(0);
                        let ring = focus::ring_style(focused);
                        let label = back_label(session.read().back());
                        rsx! {
                            button {
                                onclick: move |_| {
                                    let to = session.read().back();
                                    session.write().set_view(to);
                                },
                                style: "position:absolute;top:16px;left:16px;padding:8px 14px;border-radius:8px;border:1px solid #3a3a4c;background:rgba(18,18,26,0.82);color:#ffffff;font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);{ring}",
                                "{label}"
                            }
                        }
                    }

                    div {
                        style: "position:absolute;left:0;right:0;bottom:0;display:flex;flex-direction:column;gap:12px;padding:28px 40px;",

                        // Only true badges: the store, the install/update state,
                        // a loud anti-cheat chip when protected, and a forced
                        // build when Steam is pinned to one.
                        div {
                            style: "display:flex;flex-wrap:wrap;gap:8px;",
                            for (index, badge) in badges.iter().enumerate() {
                                {
                                    let style = badge_style(badge);
                                    let label = badge.label.clone();
                                    rsx! {
                                        span {
                                            key: "badge-{index}",
                                            style: "{style}",
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }

                        h2 {
                            style: "margin:0;font-size:var(--text-title);font-weight:700;color:#ffffff;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                            "{title}"
                        }

                        div {
                            style: "display:flex;align-items:baseline;gap:12px;font-size:var(--text-body);color:var(--text-secondary);",
                            span { "{meta_line}" }
                            if let Some(status) = status {
                                span {
                                    style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--warning);",
                                    "{status}"
                                }
                            }
                        }

                        div {
                            style: "display:flex;gap:10px;margin-top:4px;max-width:560px;",

                            if let Some(label) = playing {
                                span {
                                    style: "padding:12px 20px;border-radius:10px;border:1px solid #3a5a48;background:#243a2e;color:var(--success);font-size:var(--text-heading);",
                                    "{label}"
                                }
                            } else if primary == Primary::Play {
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
                                                    hero_play_entry.clone(),
                                                    hero_play_db.clone(),
                                                );
                                            },
                                            style: "{hero_action_style(can_play)}{ring}",
                                            {t("action.play")}
                                        }
                                    }
                                }
                            } else if primary == Primary::Install {
                                {
                                    let focused = screen_focus.index() == Some(1);
                                    let ring = focus::ring_style(focused);
                                    rsx! {
                                        button {
                                            disabled: !install_ready,
                                            onclick: move |_| {
                                                crate::launch_start::start_install(
                                                    &hero_install_game,
                                                    action_installs,
                                                    action_install_events,
                                                    action_events,
                                                    action_board,
                                                    &hero_libraries,
                                                    error,
                                                    session,
                                                );
                                            },
                                            style: "{hero_install_style(install_ready)}{ring}",
                                            "{install_label}"
                                        }
                                    }
                                }
                            } else if let Some(reason) = play_blocked {
                                // A button that cannot launch is worse than a
                                // sentence that says so.
                                span {
                                    style: "padding:12px 20px;border-radius:10px;border:1px solid #2c2c38;background:rgba(26,26,34,0.82);color:var(--text-muted);font-size:var(--text-caption);",
                                    "{reason}"
                                }
                            }
                        }
                    }
                }
            } else {
                {
                    let focused = screen_focus.index() == Some(0);
                    let ring = focus::ring_style(focused);
                    let label = back_label(session.read().back());
                    rsx! {
                        button {
                            onclick: move |_| {
                                let to = session.read().back();
                                session.write().set_view(to);
                            },
                            style: "align-self:flex-start;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);{ring}",
                            "{label}"
                        }
                    }
                }

                if let Some(hero) = hero_url.clone() {
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
                        style: "margin:0;font-size:var(--text-title);font-weight:600;color:#ffffff;",
                        "{title}"
                    }
                    if let Some(kind) = app_kind.clone() {
                        span {
                            style: "padding:2px 8px;border-radius:999px;border:1px solid #3a3a4c;color:var(--text-secondary);font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;",
                            "{kind}"
                        }
                    }
                    if let Some(status) = status {
                        span {
                            style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--warning);",
                            "{status}"
                        }
                    }
                    if let Some(label) = playing {
                        span {
                            style: "padding:8px 12px;border-radius:8px;border:1px solid #3a5a48;background:#243a2e;color:var(--success);font-size:var(--text-caption);",
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
                                    {t("action.play")}
                                }
                            }
                        }
                    } else if let Some(reason) = play_blocked {
                        // A button that cannot launch is worse than a sentence that
                        // says so; the grid's Install action is the way forward here.
                        span {
                            style: "padding:8px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1a1a22;color:var(--text-muted);font-size:var(--text-caption);",
                            "{reason}"
                        }
                    }

                    button {
                        onclick: move |_| session.write().open_graphics(graphics_game.clone()),
                        style: "padding:8px 14px;border-radius:8px;border:1px solid #3a3a4c;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                        "Graphics"
                    }
                }
            }

            if !error_text.is_empty() {
                div {
                    style: "padding:10px 14px;border-radius:8px;background:#3a1e1e;color:var(--danger);font-size:var(--text-caption);",
                    "{error_text}"
                }
            }

            div {
                style: "display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:12px;",
                {info_card(t("detail.playtime"), playtime)}
                {info_card(t("detail.last_played"), last_played)}
                {info_card(t("detail.size"), size)}
                {info_card(t("detail.install_path"), install_path)}
                {info_card(t("detail.version"), version)}
                {info_card(t("detail.anticheat"), anticheat_label)}
                if eac_show {
                    {info_card(t("detail.eac_linux_v2"), eac_label.clone())}
                }
            }

            if let Some(warning) = anticheat_warning.clone() {
                div {
                    style: "display:flex;flex-direction:column;gap:4px;padding:10px 14px;border-radius:8px;background:#3a2e1e;color:#e8c890;font-size:var(--text-caption);",
                    span {
                        style: "font-weight:600;",
                        {t("detail.anticheat_warning")}
                    }
                    span { "{warning}" }
                    for item in anticheat_evidence {
                        span {
                            style: "font-family:monospace;font-size:var(--text-micro);color:#c8a870;overflow-wrap:anywhere;",
                            "{item}"
                        }
                    }
                }
            }

            if eac_show {
                div {
                    style: "display:flex;flex-direction:column;gap:6px;padding:10px 14px;border-radius:8px;background:#1e2436;color:#c8d4f0;font-size:var(--text-caption);",
                    div {
                        style: "display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;",
                        span {
                            style: "font-weight:600;color:#ffffff;",
                            "EAC Linux v2"
                        }
                        span {
                            style: "font-size:var(--text-micro);color:#a8b4d0;",
                            "{eac_label}"
                        }
                    }
                    if let Some(message) = eac_message_text.clone() {
                        span { "{message}" }
                    }
                    if !eac_missing.is_empty() {
                        for item in eac_missing.clone() {
                            span {
                                style: "font-size:var(--text-micro);color:#a8b4d0;",
                                "• {item}"
                            }
                        }
                    }
                    if let Some(notice) = eac_notice.clone() {
                        span {
                            style: "font-size:var(--text-micro);color:var(--warning);",
                            "{notice}"
                        }
                    }
                    if let Some(argv) = eac_reboot.clone() {
                        if reboot_confirming {
                            div {
                                style: "display:flex;gap:8px;flex-wrap:wrap;",
                                button {
                                    onclick: move |_| {
                                        let mut command = std::process::Command::new(&argv[0]);
                                        command.args(&argv[1..]);
                                        match command.spawn() {
                                            Ok(_) => eac_message.set(
                                                "Rebooting into the EAC entry…".to_string(),
                                            ),
                                            Err(error) => eac_message.set(format!(
                                                "could not run `{}`: {error}",
                                                argv.join(" ")
                                            )),
                                        }
                                    },
                                    style: "padding:6px 12px;border-radius:8px;border:1px solid #4a5a8c;background:#2e3a56;color:#ffffff;font-size:var(--text-micro);cursor:pointer;",
                                    "Reboot now"
                                }
                                button {
                                    onclick: move |_| reboot_confirm.set(false),
                                    style: "padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#14141c;color:var(--text-muted);font-size:var(--text-micro);cursor:pointer;",
                                    "Cancel"
                                }
                            }
                        } else {
                            button {
                                onclick: move |_| reboot_confirm.set(true),
                                style: "align-self:flex-start;padding:6px 12px;border-radius:8px;border:1px solid #4a5a8c;background:#2e3a56;color:#ffffff;font-size:var(--text-micro);cursor:pointer;",
                                "Reboot into the EAC entry and play"
                            }
                        }
                    } else if eac_needs_entry {
                        span {
                            style: "font-family:monospace;font-size:var(--text-micro);color:#a8b4d0;overflow-wrap:anywhere;",
                            "run: {eac_command}"
                        }
                    }
                    if !eac_report.is_empty() {
                        span {
                            style: "font-size:var(--text-micro);color:#a8b4d0;",
                            "{eac_report}"
                        }
                    }
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:12px;padding-top:6px;",
                div {
                    style: "display:flex;align-items:baseline;gap:12px;",
                    h3 {
                        style: "margin:0;font-size:var(--text-body);font-weight:600;color:#ffffff;",
                        {t("detail.about")}
                    }
                    if let Some(age) = metadata_age.clone() {
                        span {
                            style: "font-size:var(--text-micro);color:var(--text-muted);",
                            {t("detail.metadata_updated").replace("{age}", &age)}
                        }
                    }
                }
                {about_body}
            }

            if let Some(hints) = in_game_hints.clone() {
                div {
                    style: "display:flex;flex-direction:column;gap:10px;padding-top:6px;",
                    div {
                        style: "display:flex;align-items:baseline;gap:12px;flex-wrap:wrap;",
                        h3 {
                            style: "margin:0;font-size:var(--text-body);font-weight:600;color:#ffffff;",
                            "In-game keys"
                        }
                        span {
                            style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--text-muted);",
                            "{hints.mode.label()}"
                        }
                    }
                    if let Some(note) = hints.note.clone() {
                        p {
                            style: "margin:0;font-size:var(--text-caption);color:var(--text-secondary);",
                            "{note}"
                        }
                    }
                    for hint in hints.hints.iter() {
                        div {
                            key: "{hint.keys}",
                            style: "display:flex;align-items:center;gap:12px;padding:10px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;",
                            span {
                                style: "font-family:monospace;font-size:var(--text-caption);color:var(--accent);min-width:88px;",
                                "{hint.keys}"
                            }
                            span {
                                style: "font-size:var(--text-caption);color:var(--text-secondary);",
                                "{hint.action}"
                            }
                        }
                    }
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:10px;padding-top:6px;",
                div {
                    style: "display:flex;align-items:baseline;gap:12px;",
                    h3 {
                        style: "margin:0;font-size:var(--text-body);font-weight:600;color:#ffffff;",
                        {t("detail.recent_launches")}
                    }
                    span {
                        style: "font-size:var(--text-micro);color:var(--text-muted);",
                        {t("detail.recent_launches_hint")}
                    }
                }
                if is_steam {
                    p {
                        style: "margin:0;font-size:var(--text-caption);color:var(--text-muted);",
                        {t("detail.steam_sessions_hint")}
                    }
                } else if sessions.is_empty() {
                    p {
                        style: "margin:0;font-size:var(--text-caption);color:var(--text-muted);",
                        {t("detail.no_sessions")}
                    }
                } else {
                    for row in sessions {
                        div {
                            key: "{row.key}",
                            style: "display:flex;align-items:center;gap:12px;padding:10px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;",
                            span {
                                style: "font-size:var(--text-caption);color:var(--text-secondary);min-width:88px;",
                                "{row.when}"
                            }
                            span {
                                style: "font-family:monospace;font-size:var(--text-micro);color:var(--accent);min-width:76px;",
                                "{row.mode}"
                            }
                            span {
                                style: if row.ok {
                                    "font-size:var(--text-caption);color:var(--success);flex:1;"
                                } else {
                                    "font-size:var(--text-caption);color:#e8c890;flex:1;"
                                },
                                "{row.verdict}"
                            }
                            if let Some(log) = row.log.clone() {
                                span {
                                    style: "font-family:monospace;font-size:var(--text-micro);color:var(--text-disabled);max-width:260px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                    "{log}"
                                }
                                CopyButton { text: log, label: t("history.copy_log_path").to_string() }
                            }
                        }
                    }
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:10px;padding-top:6px;",
                h3 {
                    style: "margin:0;font-size:var(--text-body);font-weight:600;color:#ffffff;",
                    {t("detail.profiles")}
                }
                    p {
                        style: "margin:0;font-size:var(--text-caption);color:var(--text-muted);",
                        {t("detail.profiles_hint")}
                    }

                    if !has_profiles {
                        div {
                            style: "padding:16px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;color:var(--text-muted);font-size:var(--text-caption);",
                            {t("detail.no_profiles")}
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
                                        style: "font-size:var(--text-body);color:var(--text);",
                                        "{profile_name}"
                                    }
                                    if is_active {
                                        span {
                                            style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--accent);",
                                            {t("detail.active")}
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
                                        style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                                        {t("detail.use")}
                                    }
                                    button {
                                        onclick: move |_| {
                                            let db = delete_db.clone();
                                            let game_id = delete_game.clone();
                                            let name = delete_name.clone();
                                            spawn(async move {
                                                let Some(db) = db else {
                                                    error.set(t("detail.profiles_unavailable").to_string());
                                                    return;
                                                };
                                                match persist::delete_profile(&db, &game_id, &name).await {
                                                    Ok(_) => load_profiles(Some(db), game_id, rows, selected, proton_choice, wrapper_draft, diagnostics_draft, error).await,
                                                    Err(message) => error.set(message),
                                                }
                                            });
                                        },
                                        style: "padding:6px 12px;border-radius:8px;border:1px solid #4a2c2c;background:#2a1e1e;color:#e0a0a0;font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                                        {t("detail.delete")}
                                    }
                                }
                            }
                        }
                    }

                    div {
                        style: "display:flex;gap:8px;padding-top:4px;",
                        input {
                            placeholder: t("detail.new_profile_placeholder"),
                            value: "{name}",
                            oninput: move |event| new_name.set(event.value()),
                            onfocus: move |_| session.write().set_text_focused(true),
                            onblur: move |_| session.write().set_text_focused(false),
                            style: "flex:1;padding:8px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text);outline:none;font-size:var(--text-body);",
                        }
                        button {
                            disabled: name.trim().is_empty(),
                            onclick: move |_| {
                                let name = new_name.read().trim().to_string();
                                if name.is_empty() {
                                    return;
                                }
                                spawn(create_profile(
                                    create_db.clone(),
                                    create_game.clone(),
                                    name,
                                    new_name,
                                    rows,
                                    selected,
                                    proton_choice,
                                    wrapper_draft,
                                    diagnostics_draft,
                                    error,
                                ));
                            },
                            style: create_style(name.trim().is_empty()),
                            {t("detail.save_profile")}
                        }
                    }
                }

            div {
                style: "display:flex;flex-direction:column;gap:10px;padding-top:6px;",
                h3 {
                    style: "margin:0;font-size:var(--text-body);font-weight:600;color:#ffffff;",
                    {t("detail.runtime")}
                }
                p {
                    style: "margin:0;font-size:var(--text-caption);color:var(--text-muted);",
                    if is_steam {
                        {t("detail.runtime_hint_steam")}
                    } else {
                        {t("detail.runtime_hint_other")}
                    }
                }

                    if let Some(profile) = selected_profile.clone() {
                        div {
                            style: "font-size:var(--text-micro);color:var(--text-muted);",
                            {t("detail.editing_active")}
                            span { style: "color:var(--text-secondary);", "{profile.name}" }
                            {t("detail.editing_active_suffix")}
                        }
                    } else {
                        div {
                            style: "padding:12px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;color:var(--text-muted);font-size:var(--text-caption);",
                            {t("detail.no_profile_proton")}
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
                                            style: "font-size:var(--text-body);font-weight:600;color:var(--text);",
                                            "{label}"
                                        }
                                        span {
                                            style: "font-size:var(--text-micro);color:var(--text-muted);overflow-wrap:anywhere;",
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
                                    runtime_message.set(t("detail.create_profile_first").to_string());
                                    return;
                                };
                                let Some(report) = save_report.clone() else {
                                    runtime_message.set(t("detail.proton_not_ready").to_string());
                                    return;
                                };
                                let Some(db) = save_db.clone() else {
                                    runtime_message.set(t("detail.profiles_unavailable").to_string());
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
                                            runtime_message.set(
                                                t("detail.wrote_profile")
                                                    .replace("{path}", &path.display().to_string())
                                                    .replace("{count}", &compiled.env.len().to_string())
                                                    .replace(
                                                        "{entries}",
                                                        if compiled.env.len() == 1 { "y" } else { "ies" },
                                                    ),
                                            );
                                            load_profiles(Some(db), game, rows, selected, proton_choice, wrapper_draft, diagnostics_draft, error).await;
                                        }
                                        Err(message) => runtime_message.set(message),
                                    }
                                });
                            },
                            style: action_style(save_available),
                            {t("detail.save_write_profile")}
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
                                                    runtime_message.set(
                                                        t("detail.routed_shim").replace(
                                                            "{options}",
                                                            &report.applied.unwrap_or_default(),
                                                        ),
                                                    );
                                                    routing.set(Some(runtime::routing_status(&root, &app_id)));
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(true),
                                {t("detail.route_through")}
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
                                                    runtime_message.set(
                                                        t("detail.restored_original").replace(
                                                            "{options}",
                                                            &report.restored.unwrap_or_else(|| {
                                                                t("detail.not_set").to_string()
                                                            }),
                                                        ),
                                                    );
                                                    routing.set(Some(runtime::routing_status(&root, &app_id)));
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(true),
                                {t("detail.restore_original")}
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
                                            runtime_message
                                                .set(t("detail.pick_ct_build").to_string());
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
                                                    runtime_message.set(
                                                        t("detail.steam_now_forces")
                                                            .replace("{tool}", &applied),
                                                    );
                                                    compat.set(runtime::read_compat(&root, &app_id).ok().flatten());
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(current_tool.is_some()),
                                {t("detail.force_in_steam")}
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
                                                    runtime_message
                                                        .set(t("detail.cleared_forced").to_string());
                                                    compat.set(runtime::read_compat(&root, &app_id).ok().flatten());
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(compat_status.is_some()),
                                {t("detail.clear_forced")}
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
                                                        .unwrap_or_else(|| {
                                                            t("detail.steam_decides_short").to_string()
                                                        });
                                                    runtime_message.set(
                                                        t("detail.restored_compat")
                                                            .replace("{tool}", &restored),
                                                    );
                                                    compat.set(runtime::read_compat(&root, &app_id).ok().flatten());
                                                }
                                                Err(message) => runtime_message.set(message),
                                            }
                                        });
                                    }
                                },
                                style: action_style(true),
                                {t("detail.restore_forced")}
                            }
                        }
                    }

                    if let Some(status) = routing_status.clone().filter(|_| is_steam) {
                        div {
                            style: "display:flex;flex-direction:column;gap:2px;font-size:var(--text-micro);color:var(--text-muted);",
                            span { {t("detail.steam_status").replace("{status}", &status.steam.describe())} }
                            if let Some(value) = status.launch_options.clone() {
                                span { {t("detail.launch_options").replace("{value}", &value)} }
                            } else {
                                span { {t("detail.launch_options_not_set")} }
                            }
                            if status.shim_installed {
                                span { style: "color:var(--accent);", {t("detail.routed_through_shim")} }
                            }
                            span { "{compat_label}" }
                            if let Some(warning) = status.warning.clone() {
                                span { style: "color:var(--warning);", "{warning}" }
                            }
                        }
                    }

                    div {
                        style: "display:flex;flex-direction:column;gap:6px;padding-top:4px;",
                        div {
                            style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                            span {
                                style: "font-size:var(--text-caption);font-weight:600;color:var(--text);",
                                {t("detail.command")}
                            }
                            CopyButton { text: command_text.clone() }
                            span {
                                style: "font-size:var(--text-micro);color:var(--text-muted);",
                                if is_steam {
                                    {t("detail.command_hint_steam")}
                                } else {
                                    {t("detail.command_hint_other")}
                                }
                            }
                        }
                        if let Some(message) = command_error.clone() {
                            span {
                                style: "padding:8px 12px;border-radius:8px;background:#3a2a1e;color:#e0c090;font-size:var(--text-caption);overflow-wrap:anywhere;",
                                "{message}"
                            }
                        } else {
                            code {
                                style: "display:block;padding:10px 12px;border-radius:8px;background:#14141b;border:1px solid #26262f;color:#b8c8e8;font-family:monospace;font-size:var(--text-micro);overflow-wrap:anywhere;white-space:pre-wrap;user-select:text;",
                                "{command_text}"
                            }
                        }
                        if let Some(note) = handoff_note.clone() {
                            span {
                                style: "font-size:var(--text-micro);color:var(--text-muted);",
                                {t("detail.launch_shape").replace("{note}", &note)}
                            }
                        }
                    }

                    if !runtime_note.is_empty() {
                        div {
                            style: "padding:10px 14px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:var(--text-caption);overflow-wrap:anywhere;",
                            "{runtime_note}"
                        }
                    }
                }
        }

        // The on-screen keyboard types a profile name; `Done` submits it the
        // same way the pointer's Save button does.
        if osk_open() {
            crate::osk::OskSheet {
                title: t("detail.new_profile_placeholder").to_string(),
                keyboard: osk,
                on_done: move |text| {
                    osk_open.set(false);
                    spawn(create_profile(
                        osk_db.clone(),
                        osk_game.clone(),
                        text,
                        new_name,
                        rows,
                        selected,
                        proton_choice,
                        wrapper_draft,
                        diagnostics_draft,
                        error,
                    ));
                },
                on_cancel: move |_| osk_open.set(false),
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
        error.set(t("detail.profiles_unavailable").to_string());
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

/// Create (or overwrite) one profile from a name.
///
/// The pointer's Save button and the on-screen keyboard's `Done` both call this,
/// so a controller cannot create a differently-shaped profile than a mouse. The
/// name is trimmed here because both callers pass raw field text; an empty name
/// (or one only of spaces) is ignored rather than stored.
#[allow(clippy::too_many_arguments)]
async fn create_profile(
    db: Option<Db>,
    game_id: GameId,
    name: String,
    mut new_name: Signal<String>,
    rows: Signal<Vec<StoredProfile>>,
    selected: Signal<Option<String>>,
    proton_choice: Signal<ProtonRef>,
    wrapper_draft: Signal<WrapperDraft>,
    diagnostics_draft: Signal<DiagnosticsDraft>,
    mut error: Signal<String>,
) {
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }
    let Some(db) = db else {
        error.set(t("detail.profiles_unavailable").to_string());
        return;
    };
    // A new profile starts from the Settings screen's default wrapper chain (the
    // same `[wrapper]` in `config.toml`), never a hard-coded second default. A
    // chain a hand-edited file made invalid is ignored rather than blocking the
    // profile; its own Runtime panel is where it gets set anyway.
    let profile = runtime::new_profile(game_id.clone(), name, &AppConfig::load_default().0.wrapper);
    match persist::save_profile(&db, &profile).await {
        Ok(_) => {
            new_name.set(String::new());
            load_profiles(
                Some(db),
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
        Err(message) => error.set(message),
    }
}

fn info_card(label: &str, value: String) -> Element {
    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:4px;padding:12px 14px;border-radius:10px;background:#1e1e28;border:1px solid #2c2c38;",
            span {
                style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--text-muted);",
                "{label}"
            }
            span {
                style: "font-size:var(--text-body);color:var(--text);overflow-wrap:anywhere;",
                "{value}"
            }
        }
    }
}

fn platforms_label(platforms: pitagames_core::Platforms) -> String {
    let mut names = Vec::new();
    if platforms.windows {
        names.push(t("detail.platform_windows"));
    }
    if platforms.mac {
        names.push(t("detail.platform_macos"));
    }
    if platforms.linux {
        names.push(t("detail.platform_linux"));
    }
    if names.is_empty() {
        t("detail.platforms_none").to_string()
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
        View::Home => t("detail.back_home"),
        _ => t("detail.back_library"),
    }
}

/// Start the game from the detail page.
///
/// The Play button and the focus graph's `Confirm` both call this, so a
/// controller press and a pointer click cannot drift. The actual launch is
/// [`crate::launch_start::start_play`], shared with the library grid and the home hero, so the
/// three surfaces always agree on how a Steam row and a store row start.
fn start_play(
    error: Signal<String>,
    play_signal: Signal<Play>,
    play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
    entry: GameEntry,
    db: Option<Db>,
) {
    crate::launch_start::start_play(
        &entry.game,
        entry.playtime(),
        play_signal,
        play_events,
        error,
        db,
    );
}

fn play_style(enabled: bool) -> String {
    shell::action_style(
        "margin-left:auto;padding:8px 22px;border-radius:8px;font-size:var(--text-body);",
        "border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;",
        enabled,
    )
}

/// The console hero's primary Play button.
fn hero_action_style(enabled: bool) -> String {
    shell::action_style(
        "padding:12px 28px;border-radius:10px;font-size:var(--text-heading);font-weight:600;",
        "border:1px solid #3a5a8c;background:#2e3a56;color:#ffffff;",
        enabled,
    )
}

/// The console hero's Install/Update button.
fn hero_install_style(enabled: bool) -> String {
    shell::action_style(
        "padding:12px 28px;border-radius:10px;font-size:var(--text-heading);font-weight:600;",
        "border:1px solid #3a5a48;background:#243024;color:#d8e8d0;",
        enabled,
    )
}

/// The console badge chip's inline style, by kind and tone.
///
/// The store chip reads as identity (accent), the install chip is plain unless
/// it warns, the anti-cheat chip is danger-red, and the forced-build chip is
/// neutral. Every chip reads `--text-micro` so it scales with the shell.
fn badge_style(badge: &Badge) -> String {
    let (border, background, color) = match badge.kind {
        BadgeKind::Store => ("#3f5c8c", "#243044", "#9ab0e8"),
        BadgeKind::Install => match badge.tone {
            BadgeTone::Warning => ("#5a4a2a", "#3a2e1e", "#e8c890"),
            _ => ("#3a3a4c", "#1e1e28", "var(--text-secondary)"),
        },
        BadgeKind::AntiCheat => ("#6a2a2a", "#3a1e1e", "var(--danger)"),
        BadgeKind::CompatTool => ("#3a3a4c", "#1e1e28", "var(--text-secondary)"),
    };
    format!(
        "display:inline-flex;align-items:center;padding:5px 12px;border-radius:999px;border:1px solid {border};background:{background};color:{color};font-size:var(--text-micro);font-weight:600;text-transform:uppercase;letter-spacing:0.06em;"
    )
}

fn create_style(disabled: bool) -> String {
    shell::action_style(
        "padding:8px 16px;border-radius:8px;font-size:var(--text-body);",
        "border:1px solid #3a3a4c;background:#243024;color:#d8e8d0;",
        !disabled,
    )
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
            label: t("detail.proton_auto").to_string(),
            detail: match report.and_then(|report| report.best()) {
                Some(best) => t("detail.proton_best")
                    .replace("{name}", &best.name)
                    .replace("{source}", best.source.label()),
                None => t("detail.proton_none").to_string(),
            },
            tool_name: None,
        },
        ProtonChoice {
            reference: ProtonRef::SteamDefault,
            label: t("detail.proton_steam_default").to_string(),
            detail: t("detail.proton_steam_default_detail").to_string(),
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
        "display:flex;flex-direction:column;gap:2px;align-items:flex-start;text-align:left;padding:10px 14px;border-radius:10px;border:1px solid #3f5c8c;background:#243044;cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);"
    } else {
        "display:flex;flex-direction:column;gap:2px;align-items:flex-start;text-align:left;padding:10px 14px;border-radius:10px;border:1px solid #2c2c38;background:#1e1e28;cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);"
    }
}

fn action_style(enabled: bool) -> String {
    if enabled {
        "padding:8px 16px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:var(--text-body);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);".to_string()
    } else {
        "padding:8px 16px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:var(--text-disabled);font-size:var(--text-body);cursor:default;".to_string()
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
