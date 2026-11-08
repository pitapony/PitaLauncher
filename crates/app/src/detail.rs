//! Game detail view.
//!
//! Playtime, install state and profiles come from the scan and the database;
//! the About block comes from the cached store metadata. The only side effects
//! are launching through Steam and saving/deleting profiles. Hero art and the
//! launch-options dropdown arrive with the art pipeline and the shim profile
//! writer.

use dioxus::prelude::*;

use pitagames_core::{GameId, Profile};
use pitagames_db::{Db, StoredProfile};

use crate::View;
use crate::format;
use crate::library::{self, GameEntry};
use crate::persist;

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
    /// Loopback URL for the header art, once it is on disk.
    pub hero: Option<String>,
}

/// The detail screen for one game.
#[allow(non_snake_case)]
pub fn Detail(props: DetailProps) -> Element {
    let entry = props.entry;
    let db = props.db.0;
    let mut view = props.view;
    let game_id = entry.game.id.clone();

    let rows = use_signal(Vec::<StoredProfile>::new);
    let mut selected = use_signal(|| None::<String>);
    let mut new_name = use_signal(String::new);
    let mut error = use_signal(String::new);

    // Load the profiles once per mounted game; the parent keys this component
    // by appid so opening another game remounts it.
    let db_future = db.clone();
    let game_future = game_id.clone();
    use_future(move || {
        let db = db_future.clone();
        let game_id = game_future.clone();
        async move {
            load_profiles(db, game_id, rows, selected, error).await;
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
        None => rsx! {
            p {
                style: "margin:0;font-size:12px;color:#78788a;",
                "No store metadata cached yet. Steam is asked in the background after the library loads."
            }
        },
    };

    let profile_rows = rows.read().clone();
    let has_profiles = !profile_rows.is_empty();
    let selected_name = selected.read().clone();
    let name = new_name.read().clone();
    let error_text = error.read().clone();
    let can_play = entry.can_play();

    let play_entry = entry.clone();
    let play_db = db.clone();
    let create_db = db.clone();
    let create_game = game_id.clone();

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:18px;padding:0 32px 40px;",

            button {
                onclick: move |_| view.set(View::Library),
                style: "align-self:flex-start;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:12px;cursor:pointer;",
                "← Library"
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
                button {
                    disabled: !can_play,
                    onclick: move |_| {
                        error.set(String::new());
                        let app_id = play_entry.app_id().to_string();
                        if let Err(launch_error) = library::spawn_steam(&app_id) {
                            error.set(format!("Could not ask Steam to run appid {app_id}: {launch_error}"));
                            return;
                        }
                        let Some(db) = play_db.clone() else {
                            return;
                        };
                        let id = play_entry.game.id.clone();
                        let playtime = play_entry.playtime();
                        spawn(async move {
                            if let Err(message) = persist::record_launch(&db, &id, playtime).await {
                                error.set(message);
                            }
                        });
                    },
                    style: play_style(can_play),
                    "Play"
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
                                    onclick: move |_| selected.set(Some(pick_name.clone())),
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
                                                Ok(_) => load_profiles(Some(db), game_id, rows, selected, error).await,
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
                                        load_profiles(Some(db), game_id, rows, selected, error).await;
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
        }
    }
}

/// Load (or reload) the profile list, keeping the current choice when it still
/// exists and otherwise selecting the first row.
async fn load_profiles(
    db: Option<Db>,
    game_id: GameId,
    mut rows: Signal<Vec<StoredProfile>>,
    mut selected: Signal<Option<String>>,
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
