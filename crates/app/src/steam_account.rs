//! Steam account state for the GUI: the API key, the SteamID and the owned list.
//!
//! Framework-free on purpose, like `runtime.rs`: the account view only renders
//! what this module returns, so the key handling and the refresh rules can be
//! tested without a webview.
//!
//! The key itself never leaves [`pitagames_secrets`]; this module hands out the
//! mask, the backend and the SteamID, and only exposes the raw value to the one
//! call that signs an HTTP request.

use std::path::Path;
use std::time::Duration;

use pitagames_metadata::{OwnedClient, OwnedLibrary};
use pitagames_secrets::{KeyStatus, SteamApiKey, SteamKeyStore};
use pitagames_stores::steam;
use time::OffsetDateTime;

/// How old a cached owned list may be before startup refreshes it.
///
/// Purchases are rare and the API has no documented budget; a day keeps the
/// list current without a network call on every launch. The account view has a
/// manual refresh for the "I just bought something" case.
pub const REFRESH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The Steam account as the launcher sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    /// Where the key is (or why there is none).
    pub status: KeyStatus,
    /// SteamID64 of the account Steam itself last used.
    pub steam_id: Option<String>,
    /// How that SteamID was chosen, for the UI.
    pub steam_id_source: Option<String>,
}

impl Account {
    /// Read the key and resolve the SteamID from Steam's `userdata`.
    pub fn load(steam_root: Option<&Path>) -> Self {
        let mut status = match SteamKeyStore::default_chain() {
            Some(store) => store.load().unwrap_or_else(|error| KeyStatus {
                key: None,
                backend: None,
                warnings: vec![error.to_string()],
            }),
            None => KeyStatus {
                key: None,
                backend: None,
                warnings: vec!["no config directory (HOME and XDG_CONFIG_HOME are unset)".into()],
            },
        };

        let (steam_id, steam_id_source) = match steam_root.and_then(steam::active_user_id) {
            Some(account_id) => (
                Some(steam::steam_id64(account_id)),
                Some(format!("Steam userdata · account {account_id}")),
            ),
            None => (None, None),
        };

        if status.key.is_some() && steam_id.is_none() {
            status.warnings.push(
                "no Steam account was found in userdata; sign into Steam once, then reopen PitaGames"
                    .to_string(),
            );
        }

        Self {
            status,
            steam_id,
            steam_id_source,
        }
    }

    pub fn key(&self) -> Option<&SteamApiKey> {
        self.status.key.as_ref()
    }

    /// The mask shown in the UI, e.g. `…9f3c`.
    pub fn masked_key(&self) -> Option<String> {
        self.key().map(SteamApiKey::masked)
    }

    pub fn is_configured(&self) -> bool {
        self.status.key.is_some()
    }

    /// A key and a SteamID: the owned list can be fetched.
    pub fn is_ready(&self) -> bool {
        self.is_configured() && self.steam_id.is_some()
    }

    /// The backend label for the UI.
    pub fn backend_label(&self) -> String {
        match self.status.backend {
            Some(backend) => backend.to_string(),
            None => "not configured".to_string(),
        }
    }
}

/// Validate and store a pasted key, then reload the account.
pub fn save_key(raw: &str, steam_root: Option<&Path>) -> Result<Account, String> {
    let key = SteamApiKey::parse(raw).map_err(|error| error.to_string())?;
    let store = SteamKeyStore::default_chain()
        .ok_or_else(|| "no config directory to store the key in".to_string())?;
    store.save(&key).map_err(|error| error.to_string())?;
    Ok(Account::load(steam_root))
}

/// Remove the key from every writable store, then reload the account.
pub fn forget_key(steam_root: Option<&Path>) -> Result<Account, String> {
    let store = SteamKeyStore::default_chain()
        .ok_or_else(|| "no config directory to remove the key from".to_string())?;
    store.forget().map_err(|error| error.to_string())?;
    Ok(Account::load(steam_root))
}

/// Whether a cached owned list is old enough to refresh at startup.
pub fn is_stale(fetched_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    match fetched_at {
        None => true,
        Some(fetched_at) => {
            let age = (now - fetched_at).whole_seconds();
            // A negative age means the clock moved backwards; refresh rather
            // than showing a list dated in the future forever.
            age < 0 || age >= REFRESH_AFTER.as_secs() as i64
        }
    }
}

/// Fetch the account's owned library.
pub async fn fetch_owned(key: &SteamApiKey, steam_id: &str) -> Result<OwnedLibrary, String> {
    let client = OwnedClient::new().map_err(|error| error.to_string())?;
    client
        .fetch(key.expose_secret(), steam_id)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_secrets::Backend;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    #[test]
    fn a_missing_cache_is_stale_and_a_fresh_one_is_not() {
        let now = at(2_000_000);
        assert!(is_stale(None, now));
        assert!(is_stale(Some(now - REFRESH_AFTER), now));
        assert!(!is_stale(Some(now - Duration::from_secs(3600)), now));
        assert!(
            is_stale(Some(now + Duration::from_secs(3600)), now),
            "a future timestamp means the clock moved, so refresh"
        );
    }

    #[test]
    fn an_account_without_a_key_is_not_ready() {
        let account = Account {
            status: KeyStatus::default(),
            steam_id: Some("76561197960287930".into()),
            steam_id_source: None,
        };
        assert!(!account.is_configured());
        assert!(!account.is_ready());
        assert_eq!(account.backend_label(), "not configured");
        assert_eq!(account.masked_key(), None);
    }

    #[test]
    fn a_key_without_a_steamid_cannot_fetch() {
        let account = Account {
            status: KeyStatus {
                key: Some(SteamApiKey::parse("0123456789ABCDEF0123456789ABCDEF").unwrap()),
                backend: Some(Backend::File),
                warnings: Vec::new(),
            },
            steam_id: None,
            steam_id_source: None,
        };
        assert!(account.is_configured());
        assert!(!account.is_ready());
        assert_eq!(account.masked_key().as_deref(), Some("…CDEF"));
        assert_eq!(account.backend_label(), "fallback file");
    }
}
