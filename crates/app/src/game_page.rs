//! The console game page's model: the badges it shows and the primary action it
//! offers — framework-free and unit-tested.
//!
//! The 10-foot game page states only what is *true* about a game: its store, its
//! install/update state, a loud anti-cheat badge when the game is protected, and
//! a forced-Proton-build badge when Steam is pinned to one. It never invents a
//! badge for a fact it does not have, so a couch player is not misled by a
//! placeholder. The primary action is the one rule the couch page and the
//! Continue hero share (Play, else Install, else open), so the two cannot drift.

use pitagames_core::AntiCheat;

use crate::home::store_label;
use crate::library::GameEntry;

/// How a badge reads.
///
/// The store is plain identity, a warning is amber, and the anti-cheat badge is
/// danger-red so it is noticed before anyone enables injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeTone {
    Neutral,
    Warning,
    Danger,
}

/// What a badge is about, so the renderer can style each kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    Store,
    Install,
    AntiCheat,
    CompatTool,
}

/// One true status badge on the console game page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub kind: BadgeKind,
    pub label: String,
    pub tone: BadgeTone,
}

impl Badge {
    fn new(kind: BadgeKind, label: impl Into<String>, tone: BadgeTone) -> Self {
        Self {
            kind,
            label: label.into(),
            tone,
        }
    }
}

/// The badges a game page shows, in the order it reads them.
///
/// Only true facts: the store always; the install/update word; the anti-cheat
/// runtime when `anticheat` is protected (EAC/BattlEye), never for `Unknown` or
/// `None`; and the forced compatibility tool only for a Steam row that Steam is
/// pinned to (`forced_tool` is the tool's name). A fact the page does not have is
/// an absent badge, not a placeholder.
pub fn badges(entry: &GameEntry, anticheat: AntiCheat, forced_tool: Option<&str>) -> Vec<Badge> {
    let mut badges = vec![Badge::new(
        BadgeKind::Store,
        store_label(entry.store()),
        BadgeTone::Neutral,
    )];

    // A plain install is neutral; an update, a download in flight or an unknown
    // state is a warning. `status_label` is exactly "anything but a plain
    // install", so the tone cannot drift from the word.
    let install_tone = if entry.status_label().is_some() {
        BadgeTone::Warning
    } else {
        BadgeTone::Neutral
    };
    badges.push(Badge::new(
        BadgeKind::Install,
        entry.install_status(),
        install_tone,
    ));

    if anticheat.is_protected() {
        badges.push(Badge::new(
            BadgeKind::AntiCheat,
            anticheat.label(),
            BadgeTone::Danger,
        ));
    }

    if entry.is_steam()
        && let Some(tool) = forced_tool
    {
        badges.push(Badge::new(BadgeKind::CompatTool, tool, BadgeTone::Warning));
    }

    badges
}

/// The console page's primary action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primary {
    Play,
    Install,
    Open,
}

/// What the page's one press does, given the game's install and launch state.
///
/// A game that can be played (and whose store can launch it) plays; a game whose
/// files are missing (or waiting on an update) installs, provided its store can
/// install it; anything else opens the page's details rather than offering a
/// button that could not work. The Continue hero reads the same rule, so the two
/// surfaces cannot disagree.
pub fn primary_action(entry: &GameEntry) -> Primary {
    if entry.can_play() && entry.play_is_wired() {
        Primary::Play
    } else if entry.offers_install() {
        Primary::Install
    } else {
        Primary::Open
    }
}

/// The word the primary action's button wears.
///
/// A Play button says "Play"; an Install button wears the install/update
/// **action** verb ([`GameEntry::install_action`]) while the badge wears the
/// **state** word ([`GameEntry::install_status`]). Both map from the same
/// [`GameEntry::install_state`], so the button and the badge for one game cannot
/// disagree about the fact. `Open` shows no button.
pub fn primary_label(entry: &GameEntry, primary: Primary) -> &'static str {
    match primary {
        Primary::Play => "Play",
        Primary::Install => entry.install_action(),
        Primary::Open => "",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pitagames_core::{Game, GameId, Store};
    use pitagames_stores::InstallPhase;

    use super::*;

    fn entry(store: Store, phase: InstallPhase, installed: bool, exe: Option<&str>) -> GameEntry {
        let mut game = Game::new(GameId::new(store, "1"), "A Game");
        game.installed = installed;
        game.exe = exe.map(PathBuf::from);
        GameEntry {
            game,
            phase,
            metadata: None,
            owned: false,
        }
    }

    fn labels(badges: &[Badge]) -> Vec<(&str, BadgeKind)> {
        badges
            .iter()
            .map(|badge| (badge.label.as_str(), badge.kind))
            .collect()
    }

    #[test]
    fn a_plain_installed_steam_game_shows_only_its_store_and_install_badges() {
        let entry = entry(Store::Steam, InstallPhase::Installed, true, None);
        let badges = badges(&entry, AntiCheat::Unknown, None);
        assert_eq!(
            labels(&badges),
            [
                ("Steam", BadgeKind::Store),
                ("Installed", BadgeKind::Install),
            ],
            "no placeholder badges for facts the page does not have"
        );
        assert_eq!(badges[0].tone, BadgeTone::Neutral);
        assert_eq!(badges[1].tone, BadgeTone::Neutral);
    }

    #[test]
    fn the_install_badge_names_the_update_state() {
        let update = entry(Store::Steam, InstallPhase::UpdateAvailable, true, None);
        let update_badges = badges(&update, AntiCheat::Unknown, None);
        assert_eq!(update_badges[1].label, "Update");
        assert_eq!(update_badges[1].tone, BadgeTone::Warning);

        let missing = entry(Store::Epic, InstallPhase::NotInstalled, false, None);
        let missing_badges = badges(&missing, AntiCheat::Unknown, None);
        assert_eq!(
            labels(&missing_badges),
            [
                ("Epic", BadgeKind::Store),
                ("Not installed", BadgeKind::Install),
            ]
        );
    }

    #[test]
    fn the_badge_and_the_button_share_one_install_fact() {
        // One fact, two presentations: the badge shows the state word, the
        // button the action verb, and both map from `GameEntry::install_state`,
        // so the two for one game cannot disagree.
        for phase in [InstallPhase::NotInstalled, InstallPhase::UpdateAvailable] {
            let entry = entry(Store::Steam, phase, false, None);
            let badge = badges(&entry, AntiCheat::Unknown, None)
                .into_iter()
                .find(|badge| badge.kind == BadgeKind::Install)
                .expect("every row has an install badge");
            assert_eq!(
                badge.label,
                entry.install_status(),
                "the badge shows the state"
            );
            assert_eq!(
                primary_label(&entry, Primary::Install),
                entry.install_action(),
                "the button shows the action"
            );
        }

        // An update does not block play: the primary is still Play, while the
        // same row's action verb stays "Update" for any secondary affordance.
        let update = entry(Store::Steam, InstallPhase::UpdateAvailable, true, None);
        assert_eq!(primary_action(&update), Primary::Play);
        assert_eq!(primary_label(&update, Primary::Install), "Update");
        assert_eq!(update.install_status(), "Update");
    }

    #[test]
    fn an_anti_cheat_badge_is_shown_only_when_the_game_is_protected() {
        let entry = entry(Store::Steam, InstallPhase::Installed, true, None);

        // `None` and `Unknown` are honest "nothing found" answers, not badges.
        assert_eq!(badges(&entry, AntiCheat::None, None).len(), 2);
        assert_eq!(badges(&entry, AntiCheat::Unknown, None).len(), 2);

        let eac = badges(&entry, AntiCheat::Eac, None);
        assert_eq!(eac.len(), 3);
        assert_eq!(eac[2].kind, BadgeKind::AntiCheat);
        assert_eq!(eac[2].label, "Easy Anti-Cheat");
        assert_eq!(eac[2].tone, BadgeTone::Danger, "loud, not amber");

        let battleye = badges(&entry, AntiCheat::BattlEye, None);
        assert_eq!(battleye[2].label, "BattlEye");
        assert_eq!(battleye[2].tone, BadgeTone::Danger);
    }

    #[test]
    fn a_forced_compat_tool_badge_is_shown_only_for_a_pinned_steam_game() {
        let steam = entry(Store::Steam, InstallPhase::Installed, true, None);
        let pinned = badges(&steam, AntiCheat::Unknown, Some("GE-Proton9-20"));
        assert_eq!(pinned.len(), 3);
        assert_eq!(pinned[2].kind, BadgeKind::CompatTool);
        assert_eq!(pinned[2].label, "GE-Proton9-20");
        assert_eq!(pinned[2].tone, BadgeTone::Warning);

        // A Steam game with no forced build has no badge.
        assert_eq!(badges(&steam, AntiCheat::Unknown, None).len(), 2);

        // The forced-build writer only ever touches Steam; another store with a
        // tool name still gets no badge, so the page never claims one.
        let epic = entry(
            Store::Epic,
            InstallPhase::Installed,
            true,
            Some("/games/g.exe"),
        );
        assert_eq!(
            badges(&epic, AntiCheat::Unknown, Some("GE-Proton9-20")).len(),
            2
        );
    }

    #[test]
    fn the_primary_action_follows_install_and_launch_state() {
        assert_eq!(
            primary_action(&entry(Store::Steam, InstallPhase::Installed, true, None)),
            Primary::Play
        );
        assert_eq!(
            primary_action(&entry(
                Store::Steam,
                InstallPhase::NotInstalled,
                false,
                None
            )),
            Primary::Install
        );
        // An update waiting does not block play, so the one press still plays;
        // the install badge is what tells the player an update exists.
        assert_eq!(
            primary_action(&entry(
                Store::Steam,
                InstallPhase::UpdateAvailable,
                true,
                None
            )),
            Primary::Play
        );
        // A download in flight is busy: neither play nor install until it ends.
        assert_eq!(
            primary_action(&entry(Store::Steam, InstallPhase::Downloading, true, None)),
            Primary::Open
        );
        // An installed Epic row the store cannot launch opens details rather than
        // offering a Play that could not work.
        assert_eq!(
            primary_action(&entry(Store::Epic, InstallPhase::Installed, true, None)),
            Primary::Open
        );
        assert_eq!(
            primary_action(&entry(
                Store::Epic,
                InstallPhase::Installed,
                true,
                Some("/games/Gizmo.exe"),
            )),
            Primary::Play
        );
    }

    #[test]
    fn an_install_primary_needs_a_store_that_can_install() {
        // A `Local`/imported row has no installer (`stores::installer::plan`
        // rejects it), so offering Install would only produce an error. The
        // page falls through to Open instead.
        assert_eq!(
            primary_action(&entry(
                Store::Local,
                InstallPhase::NotInstalled,
                false,
                None
            )),
            Primary::Open,
            "a row with no installer never offers Install"
        );

        // Steam installs through its own client and the three CLI stores have a
        // backend, so a missing game there still offers Install.
        for store in [Store::Steam, Store::Epic, Store::Amazon, Store::Gog] {
            assert_eq!(
                primary_action(&entry(store, InstallPhase::NotInstalled, false, None)),
                Primary::Install,
                "{store} can install"
            );
        }
    }
}
