//! XDG paths and Steam install discovery.
//!
//! Everything is Linux-shaped on purpose: PitaGames targets CachyOS/Arch, and
//! the macOS branch exists only so contributors can run the pure-Rust crates in
//! a container or on a dev Mac without the scan silently finding nothing.

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

pub fn config_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

pub fn data_dir() -> Option<PathBuf> {
    data_home().map(|base| base.join(APP_DIR))
}

pub fn cache_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache")
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
}
