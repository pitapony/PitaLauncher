//! Secret storage for store credentials.
//!
//! The only secret in M1 is the Steam Web API key. It is read from, in order:
//!
//! 1. `PITAGAMES_STEAM_API_KEY` — an explicit override for tests, CI and
//!    one-off commands. It cannot be written to, only read.
//! 2. The desktop keyring (libsecret's Secret Service on Linux, Keychain on
//!    macOS), so the key is never in a file the user has to protect.
//! 3. `$XDG_CONFIG_HOME/pitagames/secrets/steam-api-key`, mode `0600`, for
//!    machines with no session keyring (containers, headless boxes). The
//!    fallback is always announced in a warning, never silent.
//!
//! Two rules hold everywhere in this crate:
//!
//! - [`SteamApiKey`] has no `Display` and a redacting `Debug`; the string is
//!   only reachable through [`SteamApiKey::expose_secret`], so printing a
//!   struct that holds a key cannot leak it.
//! - The file is written atomically (`0600` temp file, `fsync`, `rename`), so a
//!   crash cannot leave a half-written or world-readable key behind.
//!
//! This crate is framework-free and DB-unaware; the GUI and the CLI both use it.

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use pitagames_core::paths;

/// Environment override, read before the keyring and the file.
pub const STEAM_KEY_ENV: &str = "PITAGAMES_STEAM_API_KEY";

/// Keyring service name; the account is [`STEAM_KEYRING_USER`].
pub const STEAM_KEYRING_SERVICE: &str = "pitagames";
pub const STEAM_KEYRING_USER: &str = "steam-api-key";

/// A Steam Web API key: 32 hexadecimal characters.
///
/// The wrapper exists so a key is hard to print by accident: there is no
/// `Display`, `Debug` shows only the last four characters, and the raw value
/// needs an explicit [`expose_secret`](Self::expose_secret).
#[derive(Clone, PartialEq, Eq)]
pub struct SteamApiKey(String);

impl SteamApiKey {
    /// Validate a pasted key. Surrounding whitespace is trimmed, because a
    /// copy from the Steam page usually carries a newline.
    pub fn parse(raw: &str) -> Result<Self, KeyError> {
        let trimmed = raw.trim();
        if trimmed.len() != 32 {
            return Err(KeyError::Length(trimmed.chars().count()));
        }
        if let Some(at) = trimmed.chars().position(|c| !c.is_ascii_hexdigit()) {
            return Err(KeyError::NotHex { at });
        }
        Ok(Self(trimmed.to_string()))
    }

    /// The raw key, for the one place that builds the request URL.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// A copy safe to show in the UI: `…9f3c`.
    pub fn masked(&self) -> String {
        let tail: String = self
            .0
            .chars()
            .skip(self.0.len().saturating_sub(4))
            .collect();
        format!("…{tail}")
    }
}

impl FromStr for SteamApiKey {
    type Err = KeyError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl fmt::Debug for SteamApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SteamApiKey({})", self.masked())
    }
}

/// Why a pasted string is not a Steam Web API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("a Steam Web API key is 32 hexadecimal characters, not {0}")]
    Length(usize),
    #[error("character {at} of the Steam Web API key is not a hexadecimal digit")]
    NotHex { at: usize },
}

/// Where a key was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Environment,
    Keyring,
    File,
}

impl Backend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Keyring => "keyring",
            Self::File => "fallback file",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What `load` found: the key (if any), where it came from and any warnings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyStatus {
    pub key: Option<SteamApiKey>,
    pub backend: Option<Backend>,
    /// Problems worth showing: an unavailable keyring that forced the file
    /// fallback, or an unreadable key file.
    pub warnings: Vec<String>,
}

impl KeyStatus {
    pub fn is_configured(&self) -> bool {
        self.key.is_some()
    }
}

/// One credential store.
///
/// `get` returns `Ok(None)` for "this store has no key", so a keyring that is
/// simply empty is not an error; only real failures are.
pub trait SecretBackend {
    fn get(&self) -> Result<Option<String>, Error>;
    fn set(&self, secret: &str) -> Result<(), Error>;
    fn delete(&self) -> Result<bool, Error>;
}

/// The desktop keyring, through the `keyring` crate (Secret Service/Keychain).
#[derive(Debug, Clone)]
pub struct KeyringBackend {
    service: String,
    user: String,
}

impl KeyringBackend {
    pub fn new(service: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            user: user.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, Error> {
        keyring::Entry::new(&self.service, &self.user)
            .map_err(|source| Error::Keyring(source.to_string()))
    }
}

impl SecretBackend for KeyringBackend {
    fn get(&self) -> Result<Option<String>, Error> {
        match self.entry()?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(source) => Err(Error::Keyring(source.to_string())),
        }
    }

    fn set(&self, secret: &str) -> Result<(), Error> {
        self.entry()?
            .set_password(secret)
            .map_err(|source| Error::Keyring(source.to_string()))
    }

    fn delete(&self) -> Result<bool, Error> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(source) => Err(Error::Keyring(source.to_string())),
        }
    }
}

/// A `0600` file under the config directory, used when there is no keyring.
#[derive(Debug, Clone)]
pub struct FileBackend {
    path: PathBuf,
}

impl FileBackend {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `<XDG_CONFIG_HOME>/pitagames/secrets/steam-api-key`.
    pub fn default_path() -> Option<PathBuf> {
        paths::config_dir().map(|dir| dir.join("secrets").join("steam-api-key"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SecretBackend for FileBackend {
    fn get(&self) -> Result<Option<String>, Error> {
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::Io {
                    action: "read",
                    path: self.path.clone(),
                    source,
                });
            }
        };

        let mut text = String::new();
        file.read_to_string(&mut text).map_err(|source| Error::Io {
            action: "read",
            path: self.path.clone(),
            source,
        })?;

        let text = text.trim_end_matches(['\r', '\n']).to_string();
        Ok((!text.is_empty()).then_some(text))
    }

    fn set(&self, secret: &str) -> Result<(), Error> {
        atomic_write_private(&self.path, secret)
    }

    fn delete(&self) -> Result<bool, Error> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io {
                action: "remove",
                path: self.path.clone(),
                source,
            }),
        }
    }
}

/// The key store: environment → keyring → private file.
///
/// The chain is generic over the keyring so tests can inject a failing or
/// in-memory one; production uses [`SteamKeyStore::default_chain`].
#[derive(Debug)]
pub struct SteamKeyStore<K = KeyringBackend> {
    keyring: K,
    file: FileBackend,
    env_var: String,
}

impl SteamKeyStore<KeyringBackend> {
    /// The real chain. `None` when there is no config directory to fall back to
    /// (no `HOME` and no `XDG_CONFIG_HOME`).
    pub fn default_chain() -> Option<Self> {
        Some(Self {
            keyring: KeyringBackend::new(STEAM_KEYRING_SERVICE, STEAM_KEYRING_USER),
            file: FileBackend::new(FileBackend::default_path()?),
            env_var: STEAM_KEY_ENV.to_string(),
        })
    }
}

impl<K: SecretBackend> SteamKeyStore<K> {
    pub fn new(keyring: K, file: FileBackend) -> Self {
        Self {
            keyring,
            file,
            env_var: STEAM_KEY_ENV.to_string(),
        }
    }

    /// Use another environment variable; empty means "no override".
    pub fn env_var(mut self, variable: impl Into<String>) -> Self {
        self.env_var = variable.into();
        self
    }

    /// Read the key, trying every store in order.
    ///
    /// A broken keyring is a warning, not a failure: the file fallback still
    /// works. Only an unreadable key file is an error.
    pub fn load(&self) -> Result<KeyStatus, Error> {
        let mut warnings = Vec::new();

        if !self.env_var.is_empty()
            && let Some(raw) = std::env::var_os(&self.env_var)
        {
            let raw = raw.to_string_lossy();
            match SteamApiKey::parse(&raw) {
                Ok(key) => {
                    return Ok(KeyStatus {
                        key: Some(key),
                        backend: Some(Backend::Environment),
                        warnings,
                    });
                }
                Err(error) => warnings.push(format!(
                    "{} is set but is not a valid key: {error}",
                    self.env_var
                )),
            }
        }

        match self.keyring.get() {
            Ok(Some(raw)) => match SteamApiKey::parse(&raw) {
                Ok(key) => {
                    return Ok(KeyStatus {
                        key: Some(key),
                        backend: Some(Backend::Keyring),
                        warnings,
                    });
                }
                Err(error) => warnings.push(format!(
                    "the key stored in the system keyring is not valid ({error}); ignoring it"
                )),
            },
            Ok(None) => {}
            Err(error) => warnings.push(format!(
                "the system keyring is unavailable ({error}); using the fallback file"
            )),
        }

        match self.file.get()? {
            Some(raw) => match SteamApiKey::parse(&raw) {
                Ok(key) => Ok(KeyStatus {
                    key: Some(key),
                    backend: Some(Backend::File),
                    warnings,
                }),
                Err(error) => {
                    warnings.push(format!(
                        "the key stored in {} is not valid ({error}); ignoring it",
                        self.file.path().display()
                    ));
                    Ok(KeyStatus {
                        key: None,
                        backend: None,
                        warnings,
                    })
                }
            },
            None => Ok(KeyStatus {
                key: None,
                backend: None,
                warnings,
            }),
        }
    }

    /// Store a key, preferring the keyring.
    ///
    /// Returns which store took it and any warnings. When the keyring accepts
    /// the key, a stale fallback file is removed so there is only one copy.
    pub fn save(&self, key: &SteamApiKey) -> Result<SavedKey, Error> {
        if !self.env_var.is_empty() && std::env::var_os(&self.env_var).is_some() {
            return Err(Error::EnvOverride {
                variable: self.env_var.clone(),
            });
        }

        let mut warnings = Vec::new();
        match self.keyring.set(key.expose_secret()) {
            Ok(()) => {
                if let Err(error) = self.file.delete() {
                    warnings.push(format!("could not remove the old key file: {error}"));
                }
                Ok(SavedKey {
                    backend: Backend::Keyring,
                    warnings,
                })
            }
            Err(error) => {
                warnings.push(format!(
                    "the system keyring is unavailable ({error}); the key was written to {}",
                    self.file.path().display()
                ));
                self.file.set(key.expose_secret())?;
                Ok(SavedKey {
                    backend: Backend::File,
                    warnings,
                })
            }
        }
    }

    /// Remove the key from every writable store. Returns whether one existed.
    pub fn forget(&self) -> Result<Forgotten, Error> {
        let mut warnings = Vec::new();
        let mut removed = false;

        if !self.env_var.is_empty() && std::env::var_os(&self.env_var).is_some() {
            warnings.push(format!(
                "{} is still set in the environment; it overrides the store",
                self.env_var
            ));
        }

        match self.keyring.delete() {
            Ok(true) => removed = true,
            Ok(false) => {}
            Err(error) => warnings.push(format!("could not clear the system keyring: {error}")),
        }

        if self.file.delete()? {
            removed = true;
        }

        Ok(Forgotten { removed, warnings })
    }
}

/// Where [`SteamKeyStore::save`] put the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedKey {
    pub backend: Backend,
    pub warnings: Vec<String>,
}

/// The outcome of [`SteamKeyStore::forget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forgotten {
    /// False when there was nothing to remove.
    pub removed: bool,
    pub warnings: Vec<String>,
}

/// Write a secret through a private sibling temp file.
fn atomic_write_private(path: &Path, secret: &str) -> Result<(), Error> {
    let directory = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    create_private_dir(&directory).map_err(|source| Error::Io {
        action: "create the secret directory",
        path: directory.clone(),
        source,
    })?;

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "secret".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let temp = directory.join(format!(".{name}.tmp-{}-{stamp}", std::process::id()));

    let result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(secret.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();

    if let Err(source) = result {
        let _ = fs::remove_file(&temp);
        return Err(Error::Io {
            action: "write",
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(())
}

/// Create the secret directory, mode `0700` on Unix.
fn create_private_dir(directory: &Path) -> io::Result<()> {
    if directory.is_dir() {
        return Ok(());
    }
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // `recursive` only applies the mode to the last component; tighten it
        // again in case the umask or an existing parent widened it.
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Secret-storage failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the system keyring failed: {0}")]
    Keyring(String),

    #[error("could not {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{variable} is set; unset it to manage the key here")]
    EnvOverride { variable: String },
}

/// An in-memory backend, for tests and for callers that hold no keyring.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    value: Mutex<Option<String>>,
    fail: Mutex<Option<String>>,
}

impl MemoryBackend {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: Mutex::new(Some(value.into())),
            fail: Mutex::new(None),
        }
    }

    /// Make every operation fail with this message (an unavailable keyring).
    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            value: Mutex::new(None),
            fail: Mutex::new(Some(message.into())),
        }
    }

    pub fn stored(&self) -> Option<String> {
        self.value.lock().unwrap().clone()
    }
}

impl SecretBackend for MemoryBackend {
    fn get(&self) -> Result<Option<String>, Error> {
        if let Some(message) = self.fail.lock().unwrap().clone() {
            return Err(Error::Keyring(message));
        }
        Ok(self.value.lock().unwrap().clone())
    }

    fn set(&self, secret: &str) -> Result<(), Error> {
        if let Some(message) = self.fail.lock().unwrap().clone() {
            return Err(Error::Keyring(message));
        }
        *self.value.lock().unwrap() = Some(secret.to_string());
        Ok(())
    }

    fn delete(&self) -> Result<bool, Error> {
        if let Some(message) = self.fail.lock().unwrap().clone() {
            return Err(Error::Keyring(message));
        }
        Ok(self.value.lock().unwrap().take().is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789ABCDEF0123456789ABCDEF";

    /// A chain over a temp file store and the given keyring. Each test passes
    /// its own environment variable so the tests can run in parallel.
    fn store(keyring: MemoryBackend, dir: &Path, variable: &str) -> SteamKeyStore<MemoryBackend> {
        SteamKeyStore::new(keyring, FileBackend::new(dir.join("secrets/steam-api-key")))
            .env_var(variable)
    }

    #[test]
    fn keys_are_validated_and_redacted() {
        let key = SteamApiKey::parse(&format!("  {KEY}\n")).unwrap();
        assert_eq!(key.masked(), "…CDEF");
        assert!(!format!("{key:?}").contains(KEY), "Debug must not leak");
        assert_eq!(key.expose_secret(), KEY);

        assert_eq!(SteamApiKey::parse("abc").unwrap_err(), KeyError::Length(3));
        let bad = format!("{}z{}", &KEY[..4], &KEY[5..]);
        assert_eq!(
            SteamApiKey::parse(&bad).unwrap_err(),
            KeyError::NotHex { at: 4 }
        );
    }

    #[test]
    fn the_file_fallback_writes_a_private_atomic_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets/steam-api-key");
        let file = FileBackend::new(&path);
        assert_eq!(file.get().unwrap(), None, "missing file is not an error");

        file.set(KEY).unwrap();
        assert_eq!(file.get().unwrap().as_deref(), Some(KEY));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{KEY}\n"),
            "the file is a plain key with one newline"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the key file must not be group/world readable");
            let dir_mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }

        file.set(&KEY.to_lowercase()).unwrap();
        assert_eq!(
            file.get().unwrap().as_deref(),
            Some(KEY.to_lowercase().as_str()),
            "saving again replaces the value"
        );

        assert!(file.delete().unwrap());
        assert!(!file.delete().unwrap());
        assert_eq!(file.get().unwrap(), None);
    }

    #[test]
    fn an_empty_file_is_not_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        fs::write(&path, "\n").unwrap();
        let file = FileBackend::new(&path);
        assert_eq!(file.get().unwrap(), None);
    }

    #[test]
    fn load_prefers_the_environment_then_the_keyring_then_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = FileBackend::new(dir.path().join("secrets/steam-api-key"));
        file.set(KEY).unwrap();

        let chain = store(
            MemoryBackend::new(KEY),
            dir.path(),
            "PITAGAMES_TEST_KEY_ORDER",
        );
        // SAFETY: this test owns the uniquely named variable.
        unsafe { std::env::set_var("PITAGAMES_TEST_KEY_ORDER", KEY) };
        let status = chain.load().unwrap();
        unsafe { std::env::remove_var("PITAGAMES_TEST_KEY_ORDER") };
        assert_eq!(status.backend, Some(Backend::Environment));
        assert_eq!(status.key.as_ref().unwrap().expose_secret(), KEY);

        let status = chain.load().unwrap();
        assert_eq!(status.backend, Some(Backend::Keyring));
        assert!(status.warnings.is_empty());

        fs::remove_file(dir.path().join("secrets/steam-api-key")).unwrap();
        let chain = store(
            MemoryBackend::failing("no session bus"),
            dir.path(),
            "PITAGAMES_TEST_KEY_FALLBACK",
        );
        fs::create_dir_all(dir.path().join("secrets")).unwrap();
        chain.file.set(KEY).unwrap();
        let status = chain.load().unwrap();
        assert_eq!(status.backend, Some(Backend::File));
        assert_eq!(status.warnings.len(), 1);
        assert!(status.warnings[0].contains("no session bus"));
    }

    #[test]
    fn an_invalid_environment_key_is_a_warning_not_an_override() {
        let dir = tempfile::tempdir().unwrap();
        let chain = store(
            MemoryBackend::new(KEY),
            dir.path(),
            "PITAGAMES_TEST_KEY_INVALID",
        );
        unsafe { std::env::set_var("PITAGAMES_TEST_KEY_INVALID", "nope") };
        let status = chain.load().unwrap();
        unsafe { std::env::remove_var("PITAGAMES_TEST_KEY_INVALID") };

        assert_eq!(status.backend, Some(Backend::Keyring));
        assert_eq!(status.key.as_ref().unwrap().expose_secret(), KEY);
        assert!(status.warnings[0].contains("PITAGAMES_TEST_KEY_INVALID"));
    }

    #[test]
    fn save_uses_the_keyring_and_drops_the_fallback_copy() {
        let dir = tempfile::tempdir().unwrap();
        let file = FileBackend::new(dir.path().join("secrets/steam-api-key"));
        file.set(KEY).unwrap();

        let chain = store(
            MemoryBackend::default(),
            dir.path(),
            "PITAGAMES_TEST_KEY_SAVE",
        );
        let saved = chain.save(&SteamApiKey::parse(KEY).unwrap()).unwrap();
        assert_eq!(saved.backend, Backend::Keyring);
        assert!(saved.warnings.is_empty());
        assert_eq!(file.get().unwrap(), None, "one copy only");

        assert!(chain.forget().unwrap().removed);
        assert_eq!(chain.load().unwrap(), KeyStatus::default());
    }

    #[test]
    fn save_falls_back_to_the_file_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let chain = store(
            MemoryBackend::failing("no secret service"),
            dir.path(),
            "PITAGAMES_TEST_KEY_FALLBACK_SAVE",
        );
        let saved = chain.save(&SteamApiKey::parse(KEY).unwrap()).unwrap();

        assert_eq!(saved.backend, Backend::File);
        assert_eq!(saved.warnings.len(), 1);
        assert!(saved.warnings[0].contains("no secret service"));

        let status = chain.load().unwrap();
        assert_eq!(status.backend, Some(Backend::File));
        assert_eq!(status.key.as_ref().unwrap().expose_secret(), KEY);

        // Forgetting clears the file even when the keyring is broken.
        assert!(chain.forget().unwrap().removed);
        assert!(!chain.forget().unwrap().removed);
    }

    #[test]
    fn an_environment_override_refuses_writes() {
        let dir = tempfile::tempdir().unwrap();
        let chain = store(
            MemoryBackend::default(),
            dir.path(),
            "PITAGAMES_TEST_KEY_OVERRIDE",
        );
        unsafe { std::env::set_var("PITAGAMES_TEST_KEY_OVERRIDE", KEY) };

        let error = chain
            .save(&SteamApiKey::parse(KEY).unwrap())
            .unwrap_err()
            .to_string();
        let forgotten = chain.forget().unwrap();

        unsafe { std::env::remove_var("PITAGAMES_TEST_KEY_OVERRIDE") };
        assert!(error.contains("PITAGAMES_TEST_KEY_OVERRIDE"));
        assert!(forgotten.warnings[0].contains("overrides the store"));
    }

    #[test]
    fn a_corrupt_stored_key_is_reported_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let file = FileBackend::new(dir.path().join("secrets/steam-api-key"));
        fs::create_dir_all(dir.path().join("secrets")).unwrap();
        fs::write(file.path(), "garbage\n").unwrap();

        let chain = store(
            MemoryBackend::default(),
            dir.path(),
            "PITAGAMES_TEST_KEY_CORRUPT",
        );
        let status = chain.load().unwrap();
        assert!(!status.is_configured());
        assert_eq!(status.backend, None);
        assert!(status.warnings[0].contains("not valid"));
    }
}
