//! The Steam account view: API key, SteamID and the owned-games refresh.
//!
//! The key is optional. Without one the library is "installed only", which is
//! the M0 behaviour; with one the grid also lists games the account owns but
//! Steam has not installed, each with an Install button.
//!
//! Everything here goes through `crate::steam_account` (framework-free); the
//! view only renders and forwards clicks.

use std::path::PathBuf;

use dioxus::prelude::*;

use crate::MetadataQueue;
use crate::detail::DbHandle;
use crate::library::{Library, OwnedSummary};
use crate::steam_account::{self, Account};

#[derive(Props, Clone, PartialEq)]
pub struct AccountProps {
    /// The account as last loaded; `None` while the first read is running.
    pub account: Signal<Option<Account>>,
    /// The owned side of the library state (totals, warnings, in-flight flag).
    pub owned: OwnedSummary,
    /// Steam root, for `userdata` and the SteamID.
    pub steam_root: Option<PathBuf>,
    pub db: DbHandle,
    /// Queue for the background metadata worker.
    pub metadata: Signal<Option<MetadataQueue>>,
    /// The library, so a refresh can merge its rows without a rescan.
    pub library: Signal<Option<Library>>,
}

/// One screen: what the key unlocks, how to add it, and the owned-list state.
#[allow(non_snake_case)]
pub fn AccountView(props: AccountProps) -> Element {
    let mut account_state = props.account;
    let owned = props.owned;
    let steam_root = props.steam_root;
    let db = props.db.0;
    let metadata_queue = props.metadata;
    let library_state = props.library;

    let mut key_input = use_signal(String::new);
    let mut notice = use_signal(String::new);

    let account = (*account_state.read()).clone();
    let configured = account.as_ref().is_some_and(Account::is_configured);
    let ready = account.as_ref().is_some_and(Account::is_ready);
    let warnings: Vec<String> = account
        .as_ref()
        .map(|account| account.status.warnings.clone())
        .unwrap_or_default()
        .into_iter()
        .chain(owned.warnings.iter().cloned())
        .collect();

    // Start a refresh for a specific account. The panel is shared by the
    // "Refresh" button (the loaded account) and "Save key" (the freshly saved
    // one), so the account is a parameter rather than captured state.
    let spawn_refresh = {
        let db = db.clone();
        move |account: &Account| {
            let (Some(key), Some(steam_id)) = (account.key().cloned(), account.steam_id.clone())
            else {
                return;
            };
            let mut state = library_state;
            if let Some(library) = state.write().as_mut() {
                library.owned.pending = true;
                library.owned.warnings.clear();
            }
            spawn(crate::refresh_owned(
                key,
                steam_id,
                state,
                db.clone(),
                (*metadata_queue.read()).clone(),
            ));
        }
    };

    let refresh_now = spawn_refresh.clone();
    let refresh_after_save = spawn_refresh.clone();

    // The two key buttons each own a copy: both are `move` closures.
    let save_root = steam_root.clone();
    let forget_root = steam_root.clone();

    let status_line = match &account {
        None => "reading the key store…".to_string(),
        Some(account) => match account.masked_key() {
            Some(masked) => format!("key {masked} · stored in {}", account.backend_label()),
            None => "installed only · no API key".to_string(),
        },
    };
    let steam_id_line = match &account {
        None => String::new(),
        Some(account) => match (&account.steam_id, &account.steam_id_source) {
            (Some(id), Some(source)) => format!("SteamID {id} · {source}"),
            (Some(id), None) => format!("SteamID {id}"),
            (None, _) => "no Steam account found in userdata".to_string(),
        },
    };
    let owned_line = match (owned.total, owned.fetched_at) {
        (None, _) => "the account's owned list has not been fetched yet".to_string(),
        (Some(total), Some(_)) => {
            let age = owned.fetch_age(time::OffsetDateTime::now_utc());
            match age {
                Some(age) => format!(
                    "{total} games owned · refreshed {}",
                    crate::format::duration_ago(age)
                ),
                None => format!("{total} games owned"),
            }
        }
        (Some(total), None) => format!("{total} games owned"),
    };

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:14px;padding:0 32px 40px;max-width:760px;",

            div {
                style: "display:flex;flex-direction:column;gap:6px;",
                span {
                    style: "font-size:16px;font-weight:600;color:#ffffff;",
                    "Steam account"
                }
                span {
                    style: "font-size:12px;color:#78788a;line-height:1.5;",
                    "PitaGames reads the library Steam has installed on this machine. Adding a Steam Web API key also shows the games the account owns but has not installed, so they can be installed from here."
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:6px;padding:14px 16px;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;",
                div {
                    style: "display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;",
                    span {
                        style: if configured { "font-size:13px;color:#d8e8d0;" } else { "font-size:13px;color:#c8c8d4;" },
                        "{status_line}"
                    }
                    if owned.pending {
                        span {
                            style: "font-size:11px;color:#7a9ad6;",
                            "refreshing…"
                        }
                    }
                }
                if !steam_id_line.is_empty() {
                    span {
                        style: "font-size:11px;color:#78788a;",
                        "{steam_id_line}"
                    }
                }
                span {
                    style: "font-size:11px;color:#78788a;",
                    "{owned_line}"
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:8px;padding:14px 16px;border-radius:12px;background:#1a1a22;border:1px solid #26262f;",
                span {
                    style: "font-size:10px;text-transform:uppercase;letter-spacing:0.06em;color:#78788a;",
                    "Steam Web API key"
                }
                div {
                    style: "display:flex;gap:8px;flex-wrap:wrap;align-items:center;",
                    input {
                        placeholder: "32 hexadecimal characters",
                        value: "{key_input}",
                        oninput: move |event| key_input.set(event.value()),
                        style: "flex:1;min-width:260px;padding:8px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;font-size:12px;outline:none;",
                    }
                    button {
                        disabled: key_input.read().trim().is_empty(),
                        onclick: move |_| {
                            notice.set(String::new());
                            match steam_account::save_key(&key_input(), save_root.as_deref()) {
                                Ok(updated) => {
                                    let ready = updated.is_ready();
                                    let backend = updated.backend_label();
                                    account_state.set(Some(updated.clone()));
                                    key_input.set(String::new());
                                    notice.set(format!("Key saved to the {backend}."));
                                    if ready {
                                        refresh_after_save(&updated);
                                    }
                                }
                                Err(error) => notice.set(error),
                            }
                        },
                        style: action_style(!key_input.read().trim().is_empty(), "#2e3a56"),
                        "Save key"
                    }
                    if configured {
                        button {
                            onclick: move |_| {
                                notice.set(String::new());
                                match steam_account::forget_key(forget_root.as_deref()) {
                                    Ok(updated) => {
                                        account_state.set(Some(updated));
                                        if let Some(db) = db.clone() {
                                            let mut library_state = library_state;
                                            spawn(async move {
                                                if let Err(error) = crate::persist::clear_owned(&db).await {
                                                    notice.set(error);
                                                }
                                                if let Some(library) = library_state.write().as_mut() {
                                                    library.drop_owned();
                                                }
                                            });
                                        }
                                        notice.set("Key removed. The library is installed-only again.".to_string());
                                    }
                                    Err(error) => notice.set(error),
                                }
                            },
                            style: action_style(true, "#3a2226"),
                            "Forget key"
                        }
                    }
                }
                span {
                    style: "font-size:11px;color:#78788a;line-height:1.5;",
                    "Create a key at steamcommunity.com/dev/apikey. It is stored in the desktop keyring when one is available, otherwise in a 0600 file under the config directory; it is only ever sent to api.steampowered.com."
                }
            }

            div {
                style: "display:flex;gap:8px;flex-wrap:wrap;",
                button {
                    disabled: !ready || owned.pending,
                    onclick: move |_| {
                        notice.set(String::new());
                        if let Some(account) = account.as_ref() {
                            refresh_now(account);
                        }
                    },
                    style: action_style(ready && !owned.pending, "#243024"),
                    "Refresh owned games"
                }
            }

            if !notice.read().is_empty() {
                div {
                    style: "padding:10px 14px;border-radius:8px;background:#22303a;color:#a8c8e0;font-size:12px;",
                    "{notice}"
                }
            }

            for warning in warnings {
                div {
                    key: "{warning}",
                    style: "padding:10px 14px;border-radius:8px;background:#2a2420;color:#e0b090;font-size:12px;",
                    "{warning}"
                }
            }
        }
    }
}

fn action_style(enabled: bool, background: &str) -> String {
    if enabled {
        format!(
            "padding:8px 14px;border-radius:8px;border:1px solid #3a3a4c;background:{background};color:#ffffff;font-size:12px;cursor:pointer;"
        )
    } else {
        "padding:8px 14px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:12px;cursor:default;".to_string()
    }
}
