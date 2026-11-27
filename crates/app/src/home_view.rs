//! The console home screen: the Continue hero and the shelves.
//!
//! The layout itself is [`crate::home`] — framework-free and unit-tested. This
//! module is the Dioxus seam: it draws that layout, walks its focus graph and
//! starts a game. The hero is a full-bleed Continue card for the last played
//! game; under it, one horizontally-scrolled shelf per group (Recently played,
//! Installed, then one per store).
//!
//! Two things make this more than a static list:
//!
//! - **The shelves are virtualized horizontally.** [`home::shelf_window`] gives
//!   the tiles to draw and where to place them, so a shelf with a thousand owned
//!   games mounts only what is on screen (`crate::grid` does the same
//!   vertically).
//! - **Focus drives both scroll axes.** A move resolves the next cell through
//!   the home's ragged graph, updates the focused shelf's horizontal window and
//!   slides the shelf, then pulls the whole shelf row into the vertical view.
//!   Like the library grid, the arithmetic runs *before* the render so the
//!   focused tile exists when the scroll is applied.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::html::geometry::PixelsVector2D;
use dioxus::html::{MountedData, ScrollBehavior, ScrollLogicalPosition, ScrollToOptions};
use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;

use pitagames_core::i18n::t;
use pitagames_input::Action;

use crate::View;
use crate::art;
use crate::downloads::{DownloadEvent, Downloads};
use crate::focus::{self, ActionChannel};
use crate::game_page::Primary;
use crate::home::{self, Home, HomeCell};
use crate::library::Library;
use crate::osk;
use crate::play::{Play, PlayEvent};
use crate::store_installs::{InstallEvent, Installs};

/// How wide a shelf is assumed to be before its first resize measurement, so
/// the first paint draws a sensible window instead of only the overscan tiles.
const DEFAULT_SHELF_VIEWPORT: f64 = 1_280.0;

/// Props for [`HomeView`].
#[derive(Props, Clone, PartialEq)]
pub struct HomeViewProps {
    pub library: Signal<Option<Library>>,
    /// The app's one input pump posts this screen's actions here.
    pub actions: ActionChannel,
    pub art_state: Signal<Option<std::sync::Arc<art::ArtContext>>>,
    pub art_revision: Signal<u64>,
    pub db: Signal<Option<pitagames_db::Db>>,
}

/// The mounted-element slots a focus move scrolls.
///
/// Two parallel vectors kept outside `Signal`s: a scroll target is imperative
/// DOM work, and storing a `MountedData` in a signal would make every mount a
/// render. The vectors grow to the shelf count on first mount.
type Slots = Rc<RefCell<Vec<Option<Rc<MountedData>>>>>;

fn store_slot(slots: &Slots, index: usize, data: Rc<MountedData>) {
    let mut guard = slots.borrow_mut();
    if guard.len() <= index {
        guard.resize(index + 1, None);
    }
    guard[index] = Some(data);
}

fn slot(slots: &Slots, index: usize) -> Option<Rc<MountedData>> {
    slots.borrow().get(index).and_then(|handle| handle.clone())
}

/// The shelf tile width in CSS pixels.
///
/// Fixed rather than multiplied by `--ui-scale`: the scroll arithmetic and the
/// CSS must agree on the pitch, and scaling the tiles is part of the later
/// 10-foot design pass.
fn tile_width() -> f64 {
    home::SHELF_TILE_WIDTH
}

fn hero_button_style(enabled: bool, background: &str, color: &str) -> String {
    if enabled {
        format!(
            "padding:10px 22px;border-radius:8px;border:1px solid #3a3a4c;background:{background};color:{color};font-size:var(--text-body);font-weight:600;cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);"
        )
    } else {
        "padding:10px 22px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:var(--text-disabled);font-size:var(--text-body);font-weight:600;cursor:default;"
            .to_string()
    }
}

#[allow(non_snake_case)]
pub fn HomeView(props: HomeViewProps) -> Element {
    let library = props.library;
    let art_state = props.art_state;
    let art_revision = props.art_revision;
    // The session handle reaches every concern handle (ADR-0027): the Home
    // reads the play slot, the install queue, the Downloads board and the
    // notice line through the one seam.
    let mut session = crate::context::use_session();
    let notice = session.read().notice();
    let board = session.read().downloads();
    let events = session.read().download_events();
    let db = props.db;
    let installs = session.read().installs();
    let install_events = session.read().install_events();
    let play = session.read().play();
    let play_events = session.read().play_events();

    // Reading the revision repaints a hero whose art just landed.
    let _ = art_revision();

    let screen_focus = focus::use_focus();
    // The on-screen keyboard, opened by the pad's `Search` button (or `F`). A
    // search started on the Home commits the query and lands on the Library so
    // its matches are shown.
    let mut osk_open = use_signal(|| false);
    let mut osk = use_signal(pitagames_input::OnScreenKeyboard::new);
    // One horizontal offset per shelf, and the width every shelf shares.
    let shelf_scroll = use_signal(Vec::<f64>::new);
    let viewport_width = use_signal(|| 0.0f64);
    // The mounted inner scroller (horizontal slide) and outer row (vertical
    // bring-into-view) of each shelf.
    let scrollers: Slots = use_hook(|| Rc::new(RefCell::new(Vec::new())));
    let rows: Slots = use_hook(|| Rc::new(RefCell::new(Vec::new())));
    let action_scrollers = Rc::clone(&scrollers);
    let action_rows = Rc::clone(&rows);

    // The app's one input pump posts this screen's actions here.
    let actions = props.actions;
    use_future(move || {
        let channel = actions.clone();
        let scrollers = Rc::clone(&action_scrollers);
        let rows = Rc::clone(&action_rows);
        async move {
            while let Some(action) = channel.next().await {
                if session.read().view() != View::Home {
                    continue;
                }
                // While the search sheet is open it owns every action: a
                // direction walks the key grid and `Back` cancels the edit
                // instead of leaving the Home.
                if osk_open() {
                    let press = osk.write().handle(action);
                    if press == pitagames_input::OskPress::Submit {
                        let text = osk.read().text().to_string();
                        commit_home_search(session, screen_focus, osk_open, text);
                    } else if press == pitagames_input::OskPress::Cancel {
                        osk_open.set(false);
                    }
                    continue;
                }
                // `Search` opens the keyboard seeded with the current query.
                if action == pitagames_input::Action::Search {
                    osk.set(pitagames_input::OnScreenKeyboard::with_text(
                        session.read().filter().query.clone(),
                    ));
                    osk_open.set(true);
                    continue;
                }
                perform_home_action(
                    action,
                    library,
                    session,
                    notice,
                    screen_focus,
                    viewport_width,
                    shelf_scroll,
                    scrollers.clone(),
                    rows.clone(),
                    play,
                    play_events,
                    installs,
                    install_events,
                    events,
                    board,
                    db,
                );
            }
        }
    });

    let library_guard = library.read();
    let Some(state) = library_guard.as_ref() else {
        return rsx! {
            div {
                style: "flex:1;padding:32px;color:var(--text-muted);",
                {t("library.scanning")}
            }
        };
    };
    let model = Home::build(state);
    if model.is_empty() {
        return rsx! {
            div {
                style: "flex:1;padding:48px 32px;color:var(--text-muted);",
                {t("home.empty")}
            }
        };
    }

    let art_context = art_state.read().clone();
    let offsets = shelf_scroll.read().clone();
    let width = {
        let measured = viewport_width();
        if measured > 0.0 {
            measured
        } else {
            DEFAULT_SHELF_VIEWPORT
        }
    };
    let focused = screen_focus.index();
    let play_snapshot: Play = play.read().clone();
    let installs_snapshot: Installs = (*installs.read()).clone();
    let install_ready = (*install_events.read()).is_some();
    let libraries = state.libraries.clone();
    let db_handle = (*db.read()).clone();

    // The Continue hero. Built as one `Element` so the shelves loop below stays
    // readable and the hero's buttons reuse the same launch helpers as a card.
    let hero: Option<Element> = model.hero.map(|index| {
        let entry = &state.games[index];
        let url = art_context.as_ref().and_then(|context| {
            let metadata = entry.store_metadata();
            context.url_for(
                &[
                    metadata.and_then(|metadata| metadata.header_image.as_deref()),
                    metadata.and_then(|metadata| metadata.background.as_deref()),
                ],
                pitagames_art::ArtSize::Hero,
            )
        });
        let initial = entry
            .title()
            .chars()
            .next()
            .map(|first| first.to_uppercase().to_string())
            .unwrap_or_default();
        let title = entry.title().to_string();
        let meta = entry.meta_line();
        let status = entry.status_label().unwrap_or_default();
        let playing = play_snapshot.label(&entry.game.id);
        let queued = installs_snapshot.active_label(&entry.game.id);
        let can_play = entry.can_play();
        let can_install = entry.can_install();
        let install_label = entry.install_action();
        let primary = crate::game_page::primary_action(entry);
        let play_style = hero_button_style(can_play, "#2e3a56", "#ffffff");
        let install_style = hero_button_style(install_ready, "#243024", "#d8e8d0");
        let ring = focus::ring_style(focused == Some(0));
        let open_id = entry.game.id.clone();
        let play_game = entry.game.clone();
        let play_time = entry.playtime();
        let install_id = entry.game.id.clone();
        let detail_id = entry.game.id.clone();
        let install_title = title.clone();
        let install_libraries = libraries.clone();
        let install_db = db_handle.clone();
        let is_play = matches!(primary, Primary::Play);
        let is_install = matches!(primary, Primary::Install);

        rsx! {
            div {
                style: "position:relative;height:44vh;min-height:260px;margin-bottom:22px;overflow:hidden;background:#1e1e28;border:1px solid #2c2c38;cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);{ring}",
                onclick: move |_| {
                    screen_focus.set(Some(0));
                    session.write().open_detail(open_id.clone(), View::Home);
                },

                if let Some(url) = url {
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
                    style: "position:absolute;inset:0;background:linear-gradient(90deg, rgba(12,12,18,0.94) 0%, rgba(12,12,18,0.62) 50%, rgba(12,12,18,0.12) 100%);",
                }

                div {
                    style: "position:absolute;left:0;right:0;bottom:0;display:flex;flex-direction:column;gap:10px;padding:28px 40px;",

                    span {
                        style: "font-size:var(--text-caption);font-weight:600;text-transform:uppercase;letter-spacing:0.14em;color:#8fb0e8;",
                        {t("home.continue")}
                    }
                    h2 {
                        style: "margin:0;font-size:var(--text-title);font-weight:700;color:#ffffff;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                        "{title}"
                    }
                    div {
                        style: "display:flex;align-items:baseline;gap:12px;font-size:var(--text-body);color:var(--text-secondary);",
                        span { "{meta}" }
                        if !status.is_empty() {
                            span {
                                style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--warning);",
                                "{status}"
                            }
                        }
                    }
                    div {
                        style: "display:flex;gap:10px;margin-top:6px;max-width:460px;",

                        if let Some(label) = playing {
                            span {
                                style: "padding:10px 16px;border-radius:8px;border:1px solid #3a5a48;background:#243a2e;color:var(--success);font-size:var(--text-body);",
                                "{label}"
                            }
                        } else if let Some(label) = queued {
                            span {
                                style: "padding:10px 16px;border-radius:8px;border:1px solid #3a3a4c;background:#1e1e28;color:#9ab0e8;font-size:var(--text-body);",
                                "{label}"
                            }
                        } else if is_play {
                            button {
                                disabled: !can_play,
                                onclick: move |event| {
                                    event.stop_propagation();
                                    crate::launch_start::start_play(
                                        &play_game,
                                        play_time,
                                        play,
                                        play_events,
                                        notice,
                                        install_db.clone(),
                                    );
                                },
                                style: "{play_style}",
                                {t("action.play")}
                            }
                        } else if is_install {
                            button {
                                disabled: !can_install,
                                onclick: move |event| {
                                    event.stop_propagation();
                                    crate::launch_start::start_install(
                                        &install_id,
                                        &install_title,
                                        installs,
                                        install_events,
                                        events,
                                        board,
                                        &install_libraries,
                                        notice,
                                        session,
                                    );
                                },
                                style: "{install_style}",
                                "{install_label}"
                            }
                        } else {
                            span {
                                style: "padding:10px 16px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-muted);font-size:var(--text-body);",
                                {t("home.installed_open_details")}
                            }
                        }

                        button {
                            onclick: move |event| {
                                event.stop_propagation();
                                session.write().open_detail(detail_id.clone(), View::Home);
                            },
                            style: "padding:10px 16px;border-radius:8px;border:1px solid #3a3a4c;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-body);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                            {t("action.details")}
                        }
                    }
                }
            }
        }
    });

    rsx! {
        div {
            style: "flex:1;min-height:0;overflow-y:auto;padding-bottom:24px;",

            if let Some(hero) = hero {
                {hero}
            }

            for (shelf_index, shelf) in model.shelves.iter().enumerate() {
                {
                    let title = shelf.title.clone();
                    let window = home::shelf_window(
                        shelf.games.len(),
                        tile_width(),
                        home::SHELF_GAP,
                        offsets.get(shelf_index).copied().unwrap_or(0.0),
                        width,
                        home::SHELF_OVERSCAN,
                    );
                    let visible = window.slice(&shelf.games);
                    let total_width = window.total_width;
                    let offset_x = window.offset_x;
                    let tile_w = tile_width();
                    let art_height = tile_w * 9.0 / 16.0;
                    let row_slot = Rc::clone(&rows);
                    let scroller_slot = Rc::clone(&scrollers);
                    let mut width_signal = viewport_width;

                    rsx! {
                        div {
                            key: "shelf-{shelf_index}",
                            onmounted: move |event| store_slot(&row_slot, shelf_index, event.data()),
                            style: "display:flex;flex-direction:column;gap:10px;padding:0 32px 22px;",

                            h3 {
                                style: "margin:0;font-size:var(--text-body);font-weight:600;text-transform:uppercase;letter-spacing:0.08em;color:var(--text-muted);",
                                "{title}"
                            }

                            div {
                                onmounted: move |event| store_slot(&scroller_slot, shelf_index, event.data()),
                                onresize: move |event| {
                                    if let Ok(size) = event.get_content_box_size()
                                        && size.width > 0.0
                                    {
                                        width_signal.set(size.width);
                                    }
                                },
                                // A pointer scroll (wheel, touchpad) moves the
                                // container without going through the focus
                                // pump; record it so the windowed tiles follow.
                                onscroll: move |event| {
                                    let mut shelf_scroll = shelf_scroll;
                                    let left = event.scroll_left().max(0.0);
                                    let mut offsets = shelf_scroll.read().clone();
                                    if offsets.len() <= shelf_index {
                                        offsets.resize(shelf_index + 1, 0.0);
                                    }
                                    if (offsets[shelf_index] - left).abs() > 0.5 {
                                        offsets[shelf_index] = left;
                                        shelf_scroll.set(offsets);
                                    }
                                },
                                style: "overflow-x:auto;overflow-y:hidden;padding-bottom:4px;",

                                div {
                                    style: "position:relative;width:{total_width}px;height:{art_height + 58.0}px;",

                                    div {
                                        style: "position:absolute;left:{offset_x}px;top:0;display:flex;gap:{home::SHELF_GAP}px;",

                                        for (position, game_index) in visible.iter().enumerate() {
                                            {
                                                let entry = &state.games[*game_index];
                                                let card_index = window.item_start + position;
                                                let card_cell = HomeCell::Tile {
                                                    shelf: shelf_index,
                                                    item: card_index,
                                                };
                                                let card_id = model
                                                    .id_of(card_cell)
                                                    .map(|id| id.get() as usize);
                                                let card_focused = focused == card_id;
                                                let ring = focus::ring_style(card_focused);
                                                let card_title = entry.title().to_string();
                                                let meta = home::store_label(entry.store()).to_string();
                                                let initial = card_title
                                                    .chars()
                                                    .next()
                                                    .map(|first| first.to_uppercase().to_string())
                                                    .unwrap_or_default();
                                                let url = art_context.as_ref().and_then(|context| {
                                                    let metadata = entry.store_metadata();
                                                    context.url_for(
                                                        &[
                                                            metadata.and_then(|metadata| metadata.capsule_image.as_deref()),
                                                            metadata.and_then(|metadata| metadata.header_image.as_deref()),
                                                        ],
                                                        pitagames_art::ArtSize::Grid,
                                                    )
                                                });
                                                let open_id = entry.game.id.clone();
                                                let card_key = format!("{}-{}", entry.store().as_str(), entry.app_id());

                                                rsx! {
                                                    div {
                                                        key: "{card_key}",
                                                        onclick: move |_| {
                                                            screen_focus.set(card_id);
                                                            session.write().open_detail(open_id.clone(), View::Home);
                                                        },
                                                        style: "width:{tile_w}px;flex:0 0 auto;display:flex;flex-direction:column;gap:6px;padding:10px;box-sizing:border-box;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);{ring}",

                                                        if let Some(url) = url {
                                                            img {
                                                                src: "{url}",
                                                                alt: "",
                                                                style: "width:100%;aspect-ratio:16/9;object-fit:cover;border-radius:8px;display:block;background:#2c2c38;",
                                                            }
                                                        } else {
                                                            div {
                                                                style: "width:100%;aspect-ratio:16/9;border-radius:8px;display:flex;align-items:center;justify-content:center;background:linear-gradient(135deg,#26263a,#191922);color:#414158;font-size:30px;font-weight:600;",
                                                                "{initial}"
                                                            }
                                                        }
                                                        div {
                                                            style: "font-size:var(--text-body);font-weight:600;color:#ffffff;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                                            title: "{card_title}",
                                                            "{card_title}"
                                                        }
                                                        div {
                                                            style: "font-size:var(--text-micro);color:var(--text-muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                                            "{meta}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // The on-screen keyboard is an overlay; the Home behind it keeps its
        // focus and scroll while typing.
        if osk_open() {
            osk::OskSheet {
                title: t("osk.search_library").to_string(),
                keyboard: osk,
                on_done: move |text| commit_home_search(session, screen_focus, osk_open, text),
                on_cancel: move |_| osk_open.set(false),
            }
        }
    }
}

/// Commit a search typed on the console Home.
///
/// The same model the Library's search box uses: set the query and land on the
/// Library, so a couch player who searched from the Home immediately sees the
/// matches. The sheet's pointer path and the action drain both call this.
fn commit_home_search(
    mut session: Signal<crate::session::Session>,
    screen_focus: focus::ScreenFocus,
    mut osk_open: Signal<bool>,
    text: String,
) {
    session.write().search(text);
    screen_focus.set(None);
    osk_open.set(false);
}

/// Apply one action to the home's focus graph.
///
/// Navigation resolves the next cell through [`Home::graph`], slides the focused
/// shelf horizontally and pulls its row into the vertical view. `Confirm` runs
/// the hero's primary action (Play, Install or open Details) and opens the
/// detail page for a shelf tile, exactly like the library grid's `Confirm`. It
/// is a free function because its caller is an async future that cannot capture
/// the screen's many signals in one closure.
#[allow(clippy::too_many_arguments)]
fn perform_home_action(
    action: Action,
    library: Signal<Option<Library>>,
    mut session: Signal<crate::session::Session>,
    notice: Signal<String>,
    screen_focus: focus::ScreenFocus,
    viewport_width: Signal<f64>,
    mut shelf_scroll: Signal<Vec<f64>>,
    scrollers: Slots,
    rows: Slots,
    play: Signal<Play>,
    play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
    installs: Signal<Installs>,
    install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    events: Signal<Option<UnboundedSender<DownloadEvent>>>,
    board: Signal<Downloads>,
    db: Signal<Option<pitagames_db::Db>>,
) {
    // The library borrow is held across the callbacks below; none of them touch
    // it, so a read guard here never deadlocks a write.
    let guard = library.read();
    let Some(state) = guard.as_ref() else {
        return;
    };
    let model = Home::build(state);

    if action == Action::Back {
        session.write().set_view(View::Library);
        return;
    }

    if action.is_navigation() {
        let graph = model.graph();
        let Some(id) = screen_focus.move_in(&graph, action) else {
            return;
        };

        if let Some((shelf, item)) = model.tile_of(id) {
            let measured = viewport_width();
            let width = if measured > 0.0 {
                measured
            } else {
                DEFAULT_SHELF_VIEWPORT
            };
            let offsets = shelf_scroll.read().clone();
            let current = offsets.get(shelf).copied().unwrap_or(0.0);
            let target = home::shelf_scroll_into_view(
                current,
                width,
                tile_width(),
                home::SHELF_GAP,
                item,
                home::SHELF_GAP,
            );
            if (target - current).abs() > 0.5 {
                let mut next = offsets;
                if next.len() <= shelf {
                    next.resize(shelf + 1, 0.0);
                }
                next[shelf] = target;
                shelf_scroll.set(next);
                if let Some(handle) = slot(&scrollers, shelf) {
                    let slide =
                        handle.scroll(PixelsVector2D::new(target, 0.0), ScrollBehavior::Instant);
                    spawn(async move {
                        let _ = slide.await;
                    });
                }
            }

            // Bring the whole shelf row into the vertical viewport. The row is
            // already rendered (only its tiles are windowed), so its handle is
            // live.
            if let Some(handle) = slot(&rows, shelf) {
                let reveal = handle.scroll_to_with_options(ScrollToOptions {
                    behavior: ScrollBehavior::Instant,
                    vertical: ScrollLogicalPosition::Nearest,
                    horizontal: ScrollLogicalPosition::Nearest,
                });
                spawn(async move {
                    let _ = reveal.await;
                });
            }
        }
        return;
    }

    if action != Action::Confirm {
        return;
    }

    // With nothing focused yet, the first Confirm enters on the hero.
    let Some(cell) = screen_focus.id().and_then(|id| model.cell(id)) else {
        if model.hero.is_some() {
            screen_focus.set(Some(0));
        }
        return;
    };

    match cell {
        HomeCell::Hero => {
            let Some(index) = model.hero else {
                return;
            };
            let entry = &state.games[index];
            match crate::game_page::primary_action(entry) {
                Primary::Play => crate::launch_start::start_play(
                    &entry.game,
                    entry.playtime(),
                    play,
                    play_events,
                    notice,
                    db.read().clone(),
                ),
                Primary::Install => crate::launch_start::start_install(
                    &entry.game.id,
                    entry.title(),
                    installs,
                    install_events,
                    events,
                    board,
                    &state.libraries,
                    notice,
                    session,
                ),
                Primary::Open => {
                    session
                        .write()
                        .open_detail(entry.game.id.clone(), View::Home);
                }
            }
        }
        HomeCell::Tile { .. } => {
            let Some(index) = model.game(cell) else {
                return;
            };
            session
                .write()
                .open_detail(state.games[index].game.id.clone(), View::Home);
        }
    }
}
