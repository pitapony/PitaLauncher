//! Store-provided metadata: what a storefront says a game is.
//!
//! Metadata is a cache, never the source of truth: the library comes from the
//! store's manifests, and these values only make the UI readable. Art is kept
//! as URLs here; the art pipeline downloads and resizes it separately.

use serde::{Deserialize, Serialize};

use crate::GameId;

/// What the store says this app is.
///
/// Steam calls DLC, demos, tools and videos "apps" too; knowing the kind stops
/// a soundtrack or a demo from being presented as a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppKind {
    Game,
    Dlc,
    Demo,
    Tool,
    Other(String),
}

impl AppKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Game => "game",
            Self::Dlc => "dlc",
            Self::Demo => "demo",
            Self::Tool => "tool",
            Self::Other(value) => value,
        }
    }

    pub fn is_game(&self) -> bool {
        matches!(self, Self::Game)
    }
}

impl From<&str> for AppKind {
    fn from(value: &str) -> Self {
        match value {
            "game" => Self::Game,
            "dlc" => Self::Dlc,
            "demo" => Self::Demo,
            "tool" => Self::Tool,
            other => Self::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for AppKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for AppKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AppKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from(value.as_str()))
    }
}

/// Platforms the store lists for the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Platforms {
    pub windows: bool,
    pub mac: bool,
    pub linux: bool,
}

/// Store metadata for one game, keyed by the same [`GameId`] as the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameMetadata {
    pub game: GameId,
    pub name: String,
    pub kind: AppKind,
    /// Plain-text one-liner from the store; HTML fields are not fetched.
    pub short_description: Option<String>,
    #[serde(default)]
    pub developers: Vec<String>,
    #[serde(default)]
    pub publishers: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    /// Human-readable release date exactly as the store formats it.
    pub release_date: Option<String>,
    #[serde(default)]
    pub coming_soon: bool,
    #[serde(default)]
    pub platforms: Platforms,
    pub metacritic: Option<u8>,
    pub header_image: Option<String>,
    pub capsule_image: Option<String>,
    pub background: Option<String>,
    pub website: Option<String>,
    #[serde(default)]
    pub is_free: bool,
    pub controller_support: Option<String>,
}

impl GameMetadata {
    pub fn new(game: GameId, name: impl Into<String>) -> Self {
        Self {
            game,
            name: name.into(),
            kind: AppKind::Game,
            short_description: None,
            developers: Vec::new(),
            publishers: Vec::new(),
            genres: Vec::new(),
            release_date: None,
            coming_soon: false,
            platforms: Platforms::default(),
            metacritic: None,
            header_image: None,
            capsule_image: None,
            background: None,
            website: None,
            is_free: false,
            controller_support: None,
        }
    }

    /// "Valve · Action, Adventure" — a compact line for cards and headers.
    ///
    /// Empty when the store has nothing worth showing, so callers can skip the
    /// line entirely instead of rendering a lone separator.
    pub fn byline(&self) -> String {
        let mut parts = Vec::new();
        if let Some(developer) = self.developers.first() {
            parts.push(developer.clone());
        }
        if !self.genres.is_empty() {
            parts.push(self.genres.join(", "));
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn app_kinds_round_trip_through_their_text_form() {
        for kind in [AppKind::Game, AppKind::Dlc, AppKind::Demo, AppKind::Tool] {
            assert_eq!(AppKind::from(kind.as_str()), kind);
        }
        assert_eq!(AppKind::from("music"), AppKind::Other("music".to_string()));

        let json = serde_json::to_string(&AppKind::Other("music".into())).unwrap();
        assert_eq!(json, "\"music\"");
        let parsed: AppKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AppKind::Other("music".into()));
    }

    #[test]
    fn metadata_round_trips_through_json() {
        let mut metadata = GameMetadata::new(GameId::new(Store::Steam, "620"), "Portal 2");
        metadata.short_description = Some("Puzzles.".into());
        metadata.developers = vec!["Valve".into()];
        metadata.genres = vec!["Action".into(), "Adventure".into()];
        metadata.platforms.linux = true;
        metadata.metacritic = Some(95);

        let json = serde_json::to_string(&metadata).unwrap();
        let parsed: GameMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, metadata);
        assert_eq!(parsed.byline(), "Valve · Action, Adventure");
    }

    #[test]
    fn an_empty_byline_is_empty() {
        let metadata = GameMetadata::new(GameId::new(Store::Steam, "1"), "Nothing");
        assert_eq!(metadata.byline(), "");
    }
}
