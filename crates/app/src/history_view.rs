//! The History screen: every session PitaGames started, newest first.
//!
//! Rows come from `launch_history` (SQLite) and are written by the play worker
//! when a session ends — including a launch that never started, whose row says
//! why. Steam launches are absent by design: Steam starts its own games and
//! keeps their playtime, so there is no session for the launcher to record.
//!
//! Read-only apart from **Clear history**, which deletes these rows only (the
//! library and profiles are untouched) and is two clicks on purpose.

use dioxus::prelude::*;
use pitagames_db::{Db, HistoryRepository, StoredLaunch};
use time::OffsetDateTime;

use crate::copy::CopyButton;
use crate::detail::DbHandle;
use crate::format;
use crate::persist;

/// Sessions the screen asks for. More than a person scrolls through; the CLI
/// can page further with `history --limit`.
const HISTORY_LIMIT: usize = 200;

#[derive(Props, Clone, PartialEq)]
pub struct HistoryViewProps {
    pub db: DbHandle,
    /// Bumped when a session is recorded (or the history is cleared), so the
    /// list reloads without leaving the screen.
    pub revision: Signal<u64>,
}

/// One rendered row, framework-free so the mapping is unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    /// The database row id, used as the rsx key.
    pub key: i64,
    pub when: String,
    pub title: String,
    /// `epic/Gizmo` — the store's own identity, for a title that changed.
    pub game_key: String,
    pub mode: String,
    pub verdict: String,
    pub ok: bool,
    pub log: Option<String>,
}

/// Fold one stored session into the words the screen shows.
pub fn history_row(stored: &StoredLaunch, now: OffsetDateTime) -> HistoryRow {
    let record = &stored.record;
    HistoryRow {
        key: stored.id,
        when: format::log_time(record.started_at, now),
        title: record.title.clone(),
        game_key: format!("{}/{}", record.game.store, record.game.store_id),
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
}

/// The History screen.
#[allow(non_snake_case)]
pub fn HistoryView(props: HistoryViewProps) -> Element {
    let db: Option<Db> = props.db.0;
    let mut revision = props.revision;
    let stored = use_signal(Vec::<StoredLaunch>::new);
    let mut error = use_signal(String::new);
    let mut confirming = use_signal(|| false);

    // Load on mount and whenever a session lands; reading the revision is what
    // subscribes this future to it.
    let load_db = db.clone();
    use_future(move || {
        let db = load_db.clone();
        async move {
            let _ = revision();
            reload(db, stored, error).await;
        }
    });

    let now = OffsetDateTime::now_utc();
    let rows: Vec<HistoryRow> = stored().iter().map(|row| history_row(row, now)).collect();
    let total = rows.len();
    let failures = rows.iter().filter(|row| !row.ok).count();
    let error_text = error();
    let confirming_clear = confirming();

    let refresh_db = db.clone();
    let clear_db = db.clone();
    let summary = if total == 0 {
        "No sessions recorded yet.".to_string()
    } else if failures == 0 {
        format!("{total} session(s), all exited cleanly.")
    } else {
        format!("{total} session(s), {failures} did not exit cleanly.")
    };

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:12px;padding:0 32px 40px;min-height:0;",

            div {
                style: "display:flex;flex-direction:column;gap:4px;",
                span {
                    style: "font-size:16px;font-weight:600;color:#ffffff;",
                    "History"
                }
                span {
                    style: "font-size:12px;color:#78788a;line-height:1.5;",
                    "Every launch PitaGames started for you, with its outcome and the transcript it wrote. Steam starts its own games, so its sessions are not here. Rows survive an uninstall."
                }
            }

            div {
                style: "display:flex;align-items:center;gap:12px;flex-wrap:wrap;",
                span {
                    style: "font-size:12px;color:#a8a8b8;",
                    "{summary}"
                }
                button {
                    onclick: move |_| {
                        let db = refresh_db.clone();
                        spawn(async move {
                            reload(db, stored, error).await;
                        });
                    },
                    style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:12px;cursor:pointer;",
                    "Refresh"
                }
                if confirming_clear {
                    button {
                        onclick: move |_| {
                            let db = clear_db.clone();
                            spawn(async move {
                                let Some(db) = db else {
                                    error.set("The history needs the database to be open.".to_string());
                                    return;
                                };
                                match db.clear_launches(None).await {
                                    Ok(removed) => {
                                        eprintln!("[pitagames] cleared {removed} history row(s)");
                                        confirming.set(false);
                                        revision.set(revision() + 1);
                                        reload(Some(db), stored, error).await;
                                    }
                                    Err(message) => error.set(format!("could not clear the history: {message}")),
                                }
                            });
                        },
                        style: "padding:6px 12px;border-radius:8px;border:1px solid #5a2c2c;background:#3a1e1e;color:#f0b0b0;font-size:12px;cursor:pointer;",
                        "Confirm clear"
                    }
                    button {
                        onclick: move |_| confirming.set(false),
                        style: "padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#14141c;color:#78788a;font-size:12px;cursor:pointer;",
                        "Keep"
                    }
                } else {
                    button {
                        disabled: total == 0,
                        onclick: move |_| confirming.set(true),
                        style: if total == 0 {
                            "padding:6px 12px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:12px;cursor:default;"
                        } else {
                            "padding:6px 12px;border-radius:8px;border:1px solid #4a2c2c;background:#2a1e1e;color:#e0a0a0;font-size:12px;cursor:pointer;"
                        },
                        "Clear history"
                    }
                }
            }

            if !error_text.is_empty() {
                div {
                    style: "padding:10px 14px;border-radius:8px;background:#3a1e1e;color:#e8a0a0;font-size:12px;",
                    "{error_text}"
                }
            }

            if rows.is_empty() {
                div {
                    style: "padding:16px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;color:#78788a;font-size:12px;",
                    "Play a non-Steam game and its session shows up here. A launch that fails before it starts is recorded too — that is usually the row worth reading."
                }
            } else {
                for row in rows {
                    div {
                        key: "{row.key}",
                        style: "display:flex;align-items:center;gap:12px;padding:10px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;",
                        span {
                            style: "font-size:12px;color:#a8a8b8;min-width:88px;",
                            "{row.when}"
                        }
                        div {
                            style: "display:flex;flex-direction:column;gap:2px;min-width:0;flex:1;",
                            span {
                                style: "font-size:13px;color:#e8e8ee;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                "{row.title}"
                            }
                            span {
                                style: "font-family:monospace;font-size:10px;color:#5a5a68;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                "{row.game_key}"
                            }
                        }
                        span {
                            style: "font-family:monospace;font-size:11px;color:#7a9ad6;min-width:76px;",
                            "{row.mode}"
                        }
                        span {
                            style: if row.ok {
                                "font-size:12px;color:#b8dcc4;min-width:180px;"
                            } else {
                                "font-size:12px;color:#e8c890;min-width:180px;"
                            },
                            "{row.verdict}"
                        }
                        if let Some(log) = row.log.clone() {
                            CopyButton { text: log, label: "Copy log path".to_string() }
                        }
                    }
                }
            }
        }
    }
}

/// Read the newest sessions, or report why the list cannot be read.
async fn reload(db: Option<Db>, mut stored: Signal<Vec<StoredLaunch>>, mut error: Signal<String>) {
    let Some(db) = db else {
        stored.set(Vec::new());
        error.set("The history needs the database to be open.".to_string());
        return;
    };
    match persist::recent_launches(&db, HISTORY_LIMIT).await {
        Ok(rows) => {
            error.set(String::new());
            stored.set(rows);
        }
        Err(message) => {
            error.set(message);
            stored.set(Vec::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{GameId, Store};
    use pitagames_db::LaunchRecord;
    use std::path::PathBuf;
    use std::time::Duration;

    fn stored(succeeded: bool) -> StoredLaunch {
        StoredLaunch {
            id: 7,
            record: LaunchRecord {
                game: GameId::new(Store::Epic, "Gizmo"),
                title: "Gizmo".to_string(),
                started_at: OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
                ended_at: OffsetDateTime::from_unix_timestamp(1_760_003_725).unwrap(),
                duration: Duration::from_secs(3_725),
                mode: Some("umu".to_string()),
                exit_code: succeeded.then_some(0),
                signal: None,
                succeeded,
                log_path: Some(PathBuf::from("/logs/epic-Gizmo-1.log")),
                detail: Some(if succeeded {
                    "exited 0 after 1h 02m".to_string()
                } else {
                    "no Proton build was found".to_string()
                }),
            },
        }
    }

    #[test]
    fn a_session_is_named_by_its_store_and_title() {
        let now = OffsetDateTime::from_unix_timestamp(1_760_004_000).unwrap();
        let row = history_row(&stored(true), now);

        assert_eq!(row.key, 7);
        assert_eq!(row.title, "Gizmo");
        assert_eq!(row.game_key, "epic/Gizmo");
        assert_eq!(row.mode, "umu");
        assert_eq!(row.verdict, "exited 0 after 1h 02m");
        assert!(row.ok);
        assert_eq!(row.log.as_deref(), Some("/logs/epic-Gizmo-1.log"));
    }

    #[test]
    fn a_failure_reads_as_its_reason() {
        let now = OffsetDateTime::from_unix_timestamp(1_760_004_000).unwrap();
        let row = history_row(&stored(false), now);

        assert!(!row.ok);
        assert_eq!(row.verdict, "no Proton build was found");
    }

    #[test]
    fn a_session_with_no_shape_says_so_instead_of_leaving_blanks() {
        let mut session = stored(false);
        session.record.mode = None;
        session.record.detail = None;
        session.record.log_path = None;
        let now = OffsetDateTime::from_unix_timestamp(1_760_004_000).unwrap();
        let row = history_row(&session, now);

        assert_eq!(row.mode, "not started");
        assert_eq!(row.verdict, "no verdict recorded");
        assert_eq!(row.log, None);
    }
}
