use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The storefront a game belongs to.
///
/// `Ord` is derived so a `GameId` can key a `BTreeMap` (the compat DB index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Store {
    Steam,
    Gog,
    Epic,
    Amazon,
    Local,
}

impl Store {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Steam => "steam",
            Self::Gog => "gog",
            Self::Epic => "epic",
            Self::Amazon => "amazon",
            Self::Local => "local",
        }
    }
}

impl std::fmt::Display for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Store {
    type Err = UnknownStore;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "steam" => Ok(Self::Steam),
            "gog" => Ok(Self::Gog),
            "epic" => Ok(Self::Epic),
            "amazon" => Ok(Self::Amazon),
            "local" => Ok(Self::Local),
            other => Err(UnknownStore(other.to_string())),
        }
    }
}

/// A store name in the database does not match any known storefront.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown store {0:?}")]
pub struct UnknownStore(pub String);

/// Stable identity of a game across scans.
///
/// `store_id` is the store's own identifier as a string: the Steam appid, the
/// Epic catalog id, the GOG product id, or an opaque id for local imports.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GameId {
    pub store: Store,
    pub store_id: String,
}

impl GameId {
    pub fn new(store: Store, store_id: impl Into<String>) -> Self {
        Self {
            store,
            store_id: store_id.into(),
        }
    }
}

/// Detected anti-cheat runtime.
///
/// Advisory only: injection is never enabled automatically, regardless of this
/// value. Connect-time anti-cheat is detected later; `Unknown` is the safe
/// default for a freshly scanned game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AntiCheat {
    None,
    Eac,
    BattlEye,
    #[default]
    Unknown,
}

impl AntiCheat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Eac => "eac",
            Self::BattlEye => "battleye",
            Self::Unknown => "unknown",
        }
    }

    /// The name to show a user, rather than the wire value.
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Eac => "Easy Anti-Cheat",
            Self::BattlEye => "BattlEye",
            Self::Unknown => "Not detected",
        }
    }

    /// Whether this value names an anti-cheat runtime that can punish a
    /// modified game directory.
    ///
    /// `Unknown` is deliberately *not* protected: a fresh scan cannot tell the
    /// difference between "no anti-cheat" and "not detected yet", so only a
    /// concrete `Eac`/`BattlEye` finding may hard-gate injection. `None` means
    /// the game was checked and carries none.
    pub const fn is_protected(self) -> bool {
        matches!(self, Self::Eac | Self::BattlEye)
    }
}

impl FromStr for AntiCheat {
    type Err = UnknownAntiCheat;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "eac" => Ok(Self::Eac),
            "battleye" => Ok(Self::BattlEye),
            "unknown" => Ok(Self::Unknown),
            other => Err(UnknownAntiCheat(other.to_string())),
        }
    }
}

/// An anti-cheat value in the database is not one we wrote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown anti-cheat {0:?}")]
pub struct UnknownAntiCheat(pub String);

/// A game as discovered from a store backend.
///
/// `exe` stays `None` for Steam games: Steam is launched with the appid, so the
/// executable path is not needed and may be stale after an update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Game {
    pub id: GameId,
    pub title: String,
    pub install_dir: Option<PathBuf>,
    pub exe: Option<PathBuf>,
    pub installed: bool,
    pub size_bytes: Option<u64>,
    pub version: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_updated: Option<OffsetDateTime>,
    /// When the user last launched the game, from Steam's `localconfig.vdf`
    /// or from our own process supervision for non-Steam games.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_played: Option<OffsetDateTime>,
    #[serde(with = "duration_secs", default)]
    pub playtime: Duration,
    pub anticheat: AntiCheat,
    pub tags: Vec<String>,
}

impl Game {
    pub fn new(id: GameId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            install_dir: None,
            exe: None,
            installed: false,
            size_bytes: None,
            version: None,
            last_updated: None,
            last_played: None,
            playtime: Duration::ZERO,
            anticheat: AntiCheat::Unknown,
            tags: Vec::new(),
        }
    }
}

/// A game the store says the account owns, whether or not the files are here.
///
/// This is the shape both the owned-games API client and the SQLite cache use;
/// the library merges it with the installed scan, where an installed copy wins
/// and keeps Steam's own playtime. `playtime` comes from the store (Steam counts
/// it in whole minutes), so it is the only playtime a non-installed game has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedGame {
    pub id: GameId,
    pub title: String,
    #[serde(with = "duration_secs", default)]
    pub playtime: Duration,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_played: Option<OffsetDateTime>,
}

impl OwnedGame {
    pub fn new(id: GameId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            playtime: Duration::ZERO,
            last_played: None,
        }
    }
}

/// `Duration` as whole seconds in JSON.
mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn store_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Store::Steam).unwrap(), "\"steam\"");
        assert_eq!(serde_json::to_string(&Store::Gog).unwrap(), "\"gog\"");
    }

    #[test]
    fn store_and_anticheat_round_trip_through_their_text_form() {
        for store in [
            Store::Steam,
            Store::Gog,
            Store::Epic,
            Store::Amazon,
            Store::Local,
        ] {
            assert_eq!(Store::from_str(store.as_str()).unwrap(), store);
        }
        assert_eq!(
            Store::from_str("origin").unwrap_err(),
            UnknownStore("origin".into())
        );

        for anticheat in [
            AntiCheat::None,
            AntiCheat::Eac,
            AntiCheat::BattlEye,
            AntiCheat::Unknown,
        ] {
            assert_eq!(AntiCheat::from_str(anticheat.as_str()).unwrap(), anticheat);
        }
        assert_eq!(
            AntiCheat::from_str("vanguard").unwrap_err(),
            UnknownAntiCheat("vanguard".into())
        );
    }

    #[test]
    fn only_concrete_anti_cheat_values_are_protected() {
        assert!(AntiCheat::Eac.is_protected());
        assert!(AntiCheat::BattlEye.is_protected());
        // `None` was checked; `Unknown` was not. Neither may hard-gate.
        assert!(!AntiCheat::None.is_protected());
        assert!(!AntiCheat::Unknown.is_protected());

        assert_eq!(AntiCheat::Eac.label(), "Easy Anti-Cheat");
        assert_eq!(AntiCheat::BattlEye.label(), "BattlEye");
        assert_eq!(AntiCheat::None.label(), "None");
        assert_eq!(AntiCheat::Unknown.label(), "Not detected");
    }

    #[test]
    fn game_round_trips_through_json() {
        let mut game = Game::new(GameId::new(Store::Steam, "570"), "Dota 2");
        game.installed = true;
        game.playtime = Duration::from_secs(90);

        let json = serde_json::to_string(&game).unwrap();
        assert!(json.contains("\"playtime\":90"), "{json}");
        assert!(json.contains("\"store\":\"steam\""), "{json}");

        let parsed: Game = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, game);
    }
}
