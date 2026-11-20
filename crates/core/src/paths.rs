//! XDG paths and Steam install discovery.
//!
//! Everything is Linux-shaped on purpose: PitaGames targets CachyOS/Arch, and
//! the macOS branch exists only so contributors can run the pure-Rust crates in
//! a container or on a dev Mac without the scan silently finding nothing.

use std::env;
use std::path::PathBuf;

const APP_DIR: &str = "pitagames";

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn home_dir() -> Option<PathBuf> {
    env_path("HOME")
}

fn xdg_dir(variable: &str, fallback: &str) -> Option<PathBuf> {
    env_path(variable)
        .or_else(|| home_dir().map(|home| home.join(fallback)))
        .map(|base| base.join(APP_DIR))
}

/// The XDG data base directory *without* the `pitagames` suffix.
///
/// Needed by anything that reads another application's XDG data (umu's Proton
/// builds live in `$XDG_DATA_HOME/umu`), which is why it is separate from
/// [`data_dir`].
pub fn data_home() -> Option<PathBuf> {
    env_path("XDG_DATA_HOME").or_else(|| home_dir().map(|home| home.join(".local/share")))
}

/// The XDG config base directory *without* the `pitagames` suffix.
///
/// Needed by anything that reads another application's XDG config: `nile`
/// keeps its library cache and install records in `$XDG_CONFIG_HOME/nile`,
/// which is why this is separate from [`config_dir`].
pub fn config_home() -> Option<PathBuf> {
    env_path("XDG_CONFIG_HOME").or_else(|| home_dir().map(|home| home.join(".config")))
}

pub fn config_dir() -> Option<PathBuf> {
    config_home().map(|base| base.join(APP_DIR))
}

pub fn data_dir() -> Option<PathBuf> {
    data_home().map(|base| base.join(APP_DIR))
}

pub fn cache_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache")
}

/// `<XDG_DATA_HOME>/pitagames/profiles`: the pre-compiled shim profiles.
pub fn profiles_dir() -> Option<PathBuf> {
    data_dir().map(|base| base.join("profiles"))
}

/// `<XDG_DATA_HOME>/pitagames/games`: where non-Steam stores install by default.
///
/// A store backend appends its own namespace and the game's store id
/// (`games/epic/<app name>`), so the path is predictable before an install
/// starts and survives a legendary config wipe. A user can always pass another
/// directory; this is only the default.
pub fn games_dir() -> Option<PathBuf> {
    data_dir().map(|base| base.join("games"))
}

/// One filesystem-safe path component built from untrusted text.
///
/// A store's app title (or a command line) becomes exactly one component of an
/// install directory, so separators and control characters turn into `-`, and
/// an empty result, `.` or `..` falls back to `fallback` instead of letting the
/// path escape the directory the caller chose.
pub fn safe_component(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' => '-',
            ch if ch.is_control() => '-',
            ch => ch,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// `<XDG_DATA_HOME>/pitagames/pitagames.lock`: the single-instance lock file.
///
/// It sits next to the database because that is the resource two instances would
/// race over. The file is never deleted and only ever carries the holder's pid;
/// the lock itself is the `flock`, not the file's existence.
pub fn lock_file() -> Option<PathBuf> {
    data_dir().map(|base| base.join("pitagames.lock"))
}

/// `<XDG_DATA_HOME>/pitagames/logs`.
pub fn logs_dir() -> Option<PathBuf> {
    data_dir().map(|base| base.join("logs"))
}

/// `<XDG_DATA_HOME>/pitagames/prefixes`: one Wine/Proton prefix per game.
///
/// Steam keeps its own prefixes under `steamapps/compatdata/<appid>`; every
/// non-Steam store gets a directory here instead (`prefixes/<store>-<id>`),
/// because pointing two games at one prefix is how a launcher overwrites
/// another game's registry and saves. The layout and the umu environment that
/// uses it live in `platform::prefix`.
pub fn prefixes_dir() -> Option<PathBuf> {
    data_dir().map(|base| base.join("prefixes"))
}

/// `<XDG_DATA_HOME>/pitagames/instance.sock`: the activation socket.
///
/// The second launch connects here to raise the running window; see
/// [`crate::activation`]. It lives next to the lock because both describe the
/// same "one launcher at a time" contract.
pub fn activation_socket() -> Option<PathBuf> {
    data_dir().map(|base| base.join("instance.sock"))
}

/// `<XDG_DATA_HOME>/applications/pitagames.desktop`: the generated entry.
pub fn desktop_entry_file() -> Option<PathBuf> {
    data_home().map(|base| base.join("applications").join("pitagames.desktop"))
}

/// The XDG data search path (`XDG_DATA_DIRS`, default
/// `/usr/local/share:/usr/share`).
///
/// Used to notice a packaged `.desktop` file before generating a user-level one
/// that would shadow it.
pub fn data_dirs() -> Vec<PathBuf> {
    match env_path("XDG_DATA_DIRS") {
        Some(dirs) => env::split_paths(&dirs).collect(),
        None => vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ],
    }
}

/// `<XDG_DATA_HOME>/pitagames/backups/steam`: Steam config backups and the
/// restore journals.
pub fn steam_backup_dir() -> Option<PathBuf> {
    data_dir().map(|base| base.join("backups").join("steam"))
}

/// `<XDG_CONFIG_HOME>/pitagames/input.toml`: per-device controller/keyboard maps.
///
/// The maps are their own file rather than a section of a future `config.toml`
/// because a device GUID is an awkward table key and because the file is owned
/// entirely by `platform`-free input code. Nothing reads it unless the GUI wires
/// it up; see `pitagames-input`.
pub fn input_maps_file() -> Option<PathBuf> {
    config_dir().map(|base| base.join("input.toml"))
}

/// `<XDG_CONFIG_HOME>/pitagames/config.toml`: the launcher's own preferences.
///
/// The two shells, the start mode and the display numbers. A missing file means
/// the shipped defaults; the app writes it when the player changes a preference.
pub fn config_file() -> Option<PathBuf> {
    config_dir().map(|base| base.join("config.toml"))
}

/// `<XDG_CONFIG_HOME>/pitagames/compat`: user overrides of the shipped compat DB.
///
/// A file here with the same game id as a shipped entry overrides it field by
/// field; a game with no shipped entry is added to the database. The shipped
/// entries live at `/usr/share/pitagames/compat` (and a synced copy under the
/// data directory), never here.
pub fn compat_dir() -> Option<PathBuf> {
    config_dir().map(|base| base.join("compat"))
}

/// Candidate Steam roots, most likely first, without checking existence.
///
/// `PITAGAMES_STEAM_ROOT` (set and non-empty) always comes first, so a user
/// with Steam (or a whole library) in an arbitrary place can aim the scans
/// from the environment without touching config files. The two Linux entries
/// cover the native client: `~/.local/share/Steam` is the real library,
/// `~/.steam/steam` is the compatibility symlink.
pub fn steam_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(root) = env_path("PITAGAMES_STEAM_ROOT") {
        roots.push(root);
    }

    let Some(home) = home_dir() else {
        return roots;
    };

    if cfg!(target_os = "linux") {
        roots.push(home.join(".local/share/Steam"));
        roots.push(home.join(".steam/steam"));
    } else if cfg!(target_os = "macos") {
        roots.push(home.join("Library/Application Support/Steam"));
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn xdg_dirs_end_with_the_app_name() {
        for dir in [config_dir(), data_dir(), cache_dir()]
            .into_iter()
            .flatten()
        {
            assert_eq!(dir.file_name().unwrap(), APP_DIR);
        }
    }

    #[test]
    fn data_home_is_the_xdg_base_without_the_app_suffix() {
        let guard = ENV_LOCK.lock().unwrap();

        // SAFETY: the only tests mutating XDG_DATA_HOME run under this lock.
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-data") };
        let home = data_home();
        let app = data_dir();
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        drop(guard);

        assert_eq!(home, Some(PathBuf::from("/tmp/xdg-data")));
        assert_eq!(app, Some(PathBuf::from("/tmp/xdg-data/pitagames")));
    }

    #[test]
    fn env_override_is_the_first_steam_root() {
        let guard = ENV_LOCK.lock().unwrap();

        // SAFETY: the only test mutating PITAGAMES_STEAM_ROOT runs under the
        // same static lock, and nothing else reads this variable here.
        unsafe { std::env::set_var("PITAGAMES_STEAM_ROOT", "/games/Steam79") };
        let roots = steam_roots();
        unsafe { std::env::remove_var("PITAGAMES_STEAM_ROOT") };
        drop(guard);

        assert_eq!(
            roots.first().and_then(|root| root.to_str()),
            Some("/games/Steam79")
        );
    }

    #[test]
    fn empty_env_override_is_ignored() {
        let guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("PITAGAMES_STEAM_ROOT", "") };
        let roots = steam_roots();
        unsafe { std::env::remove_var("PITAGAMES_STEAM_ROOT") };
        drop(guard);

        assert!(!roots.iter().any(|root| root.as_os_str().is_empty()));
    }

    #[test]
    fn desktop_and_activation_files_live_under_the_data_home() {
        let guard = ENV_LOCK.lock().unwrap();

        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-desktop") };
        let entry = desktop_entry_file();
        let socket = activation_socket();
        let lock = lock_file();
        let games = games_dir();
        let prefixes = prefixes_dir();
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        drop(guard);

        assert_eq!(
            games,
            Some(PathBuf::from("/tmp/xdg-desktop/pitagames/games"))
        );
        assert_eq!(
            prefixes,
            Some(PathBuf::from("/tmp/xdg-desktop/pitagames/prefixes"))
        );
        assert_eq!(
            entry,
            Some(PathBuf::from(
                "/tmp/xdg-desktop/applications/pitagames.desktop"
            ))
        );
        assert_eq!(
            socket,
            Some(PathBuf::from("/tmp/xdg-desktop/pitagames/instance.sock"))
        );
        // Both single-instance files sit in the same directory.
        assert_eq!(socket.unwrap().parent(), lock.unwrap().parent());
    }

    #[test]
    fn the_data_search_path_follows_xdg_data_dirs() {
        let guard = ENV_LOCK.lock().unwrap();

        unsafe { std::env::set_var("XDG_DATA_DIRS", "/opt/one:/opt/two") };
        let dirs = data_dirs();
        unsafe { std::env::remove_var("XDG_DATA_DIRS") };
        let defaults = data_dirs();
        drop(guard);

        assert_eq!(dirs, [PathBuf::from("/opt/one"), PathBuf::from("/opt/two")]);
        assert_eq!(
            defaults,
            [
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share")
            ]
        );
    }

    #[test]
    fn config_home_is_the_xdg_base_without_the_app_suffix() {
        let guard = ENV_LOCK.lock().unwrap();

        // SAFETY: the only tests mutating XDG_CONFIG_HOME run under this lock.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-config") };
        let home = config_home();
        let app = config_dir();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        drop(guard);

        assert_eq!(home, Some(PathBuf::from("/tmp/xdg-config")));
        assert_eq!(app, Some(PathBuf::from("/tmp/xdg-config/pitagames")));
    }

    #[test]
    fn the_input_maps_file_sits_next_to_the_config_dir() {
        let guard = ENV_LOCK.lock().unwrap();

        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-input") };
        let maps = input_maps_file();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        drop(guard);

        assert_eq!(
            maps,
            Some(PathBuf::from("/tmp/xdg-input/pitagames/input.toml"))
        );
    }

    #[test]
    fn a_sanitized_component_cannot_escape_its_directory() {
        assert_eq!(safe_component("Gizmo", "game"), "Gizmo");
        assert_eq!(safe_component("  Adios  ", "game"), "Adios");
        assert_eq!(
            safe_component("../../etc/passwd", "game"),
            "..-..-etc-passwd"
        );
        assert_eq!(safe_component("a/b\\c", "game"), "a-b-c");
        assert_eq!(safe_component("tab\there", "game"), "tab-here");
        assert_eq!(safe_component("..", "game"), "game");
        assert_eq!(safe_component(".", "game"), "game");
        assert_eq!(safe_component("   ", "game"), "game");
    }
}
