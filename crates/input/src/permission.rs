//! Whether a controller is unusable because of permissions.
//!
//! `gilrs` opens `/dev/input/event*` through udev; on a normal Arch/CachyOS
//! session logind's `uaccess` tag grants the active user access. When it does
//! not — the user is not in the `input` group, or the ACL was never applied —
//! the pad is silently absent and the launcher looks broken. This module is the
//! read-only probe behind the one-line fix the Controls screen shows.
//!
//! It never grabs a device: the only thing it does to a node is a read-only
//! `open`, so a game can keep reading the same node while the launcher probes.
//! No `EVIOCGRAB` anywhere.

use std::path::{Path, PathBuf};

/// The Arch/CachyOS group whose membership grants raw input access.
pub const INPUT_GROUP: &str = "input";

/// The fix shown when the user is not in the `input` group.
const ADD_TO_INPUT_GROUP: &str = "A controller is present but the launcher may not read it. \
Add your user to the input group: sudo usermod -aG input $USER — then log out and back in.";

/// The fix shown when the user *is* in the group but the node was not opened,
/// which points at logind's `uaccess` tag not having been applied.
const UACCESS_DID_NOT_APPLY: &str = "A controller is present but the launcher may not read it. \
Replug the controller, or log out and back in so logind's uaccess grant applies.";

/// The fix when the user's group membership could not be read, naming both causes.
const UNKNOWN_CAUSE: &str = "A controller is present but the launcher may not read it. \
Add your user to the input group (sudo usermod -aG input $USER) or replug it, then log back in.";

/// Whether the user belongs to the [`INPUT_GROUP`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupState {
    /// The user is in the `input` group.
    Member,
    /// The user is not in the `input` group.
    NotMember,
    /// Membership could not be determined (no `/proc`, an unusual NSS setup).
    Unknown,
}

/// The launcher's read access to the raw input devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadAccess {
    /// At least one event device can be opened read-only: a pad can be handed over.
    Usable,
    /// Event devices exist but none can be opened — a permission problem.
    Denied {
        /// How many `event*` nodes were seen.
        devices: usize,
        /// The user's `input` group membership, for the fix's wording.
        group: GroupState,
    },
    /// No event device exists: nothing is plugged in (or there is no evdev).
    NoDevices,
}

impl PadAccess {
    /// The one-line, actionable notice for the Controls screen, if any.
    ///
    /// `None` for a usable or an empty layout: a missing pad is not a
    /// permission problem, so the screen stays quiet rather than guessing.
    pub const fn message(&self) -> Option<&'static str> {
        match self {
            Self::Usable | Self::NoDevices => None,
            Self::Denied { group, .. } => Some(match group {
                GroupState::Member => UACCESS_DID_NOT_APPLY,
                GroupState::NotMember => ADD_TO_INPUT_GROUP,
                GroupState::Unknown => UNKNOWN_CAUSE,
            }),
        }
    }

    /// Whether a pad is present but unreachable for permission reasons.
    pub const fn is_denied(&self) -> bool {
        matches!(self, Self::Denied { .. })
    }
}

/// The facts a probe reads, so a test can supply a fake `/dev/input` layout.
///
/// The real implementation is [`DevInput`]; tests script the nodes and the
/// access they grant. Splitting it out is what makes "denied" testable on a
/// machine whose own `/dev/input` is absent (macOS) or permissive (a container
/// running as root).
pub trait InputLayout {
    /// The `event*` device nodes that exist, sorted.
    fn event_devices(&self) -> Vec<PathBuf>;

    /// Whether this process can open `device` read-only. Never grabs.
    fn readable(&self, device: &Path) -> bool;

    /// The user's [`INPUT_GROUP`] membership.
    fn input_group(&self) -> GroupState;
}

/// Assess a layout: usable when any node is readable, denied when nodes exist
/// but none is, and neither when there is no node at all.
///
/// Ordered so a working pad can never produce a false positive: a single
/// readable node short-circuits to [`PadAccess::Usable`].
pub fn assess(layout: &dyn InputLayout) -> PadAccess {
    let devices = layout.event_devices();
    if devices.is_empty() {
        return PadAccess::NoDevices;
    }
    if devices.iter().any(|device| layout.readable(device)) {
        return PadAccess::Usable;
    }
    PadAccess::Denied {
        devices: devices.len(),
        group: layout.input_group(),
    }
}

/// The real `/dev/input` layout.
#[derive(Debug, Clone)]
pub struct DevInput {
    root: PathBuf,
}

impl DevInput {
    /// The default `/dev/input`.
    pub fn new() -> Self {
        Self::with_root("/dev/input")
    }

    /// A layout rooted elsewhere (a fixture, or a test's tempdir).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Default for DevInput {
    fn default() -> Self {
        Self::new()
    }
}

impl InputLayout for DevInput {
    fn event_devices(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut devices: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("event"))
            })
            .collect();
        devices.sort();
        devices
    }

    fn readable(&self, device: &Path) -> bool {
        // A read-only `open` is the whole test. No `EVIOCGRAB`, no ioctl: while
        // the launcher checks a node a running game keeps reading it.
        std::fs::File::open(device).is_ok()
    }

    fn input_group(&self) -> GroupState {
        let Some(groups) = process_groups() else {
            return GroupState::Unknown;
        };
        match group_gid(INPUT_GROUP) {
            Some(gid) if groups.contains(&gid) => GroupState::Member,
            Some(_) => GroupState::NotMember,
            None => GroupState::Unknown,
        }
    }
}

/// The process's supplementary group ids, from `/proc/self/status`.
///
/// `/proc` is Linux-only; elsewhere (macOS) this is `None` and the caller
/// reports [`GroupState::Unknown`].
fn process_groups() -> Option<Vec<u32>> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("Groups:"))?;
    Some(
        line["Groups:".len()..]
            .split_whitespace()
            .filter_map(|value| value.parse::<u32>().ok())
            .collect(),
    )
}

/// A named group's gid, from `/etc/group`.
fn group_gid(name: &str) -> Option<u32> {
    let groups = std::fs::read_to_string("/etc/group").ok()?;
    for line in groups.lines() {
        let mut fields = line.split(':');
        if fields.next() == Some(name) {
            return fields.nth(1)?.parse::<u32>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted `/dev/input`: explicit nodes and explicit read access.
    struct FakeLayout {
        devices: Vec<PathBuf>,
        readable: Vec<PathBuf>,
        group: GroupState,
    }

    impl FakeLayout {
        fn new(group: GroupState) -> Self {
            Self {
                devices: Vec::new(),
                readable: Vec::new(),
                group,
            }
        }

        fn device(mut self, name: &str, readable: bool) -> Self {
            let path = PathBuf::from(name);
            self.devices.push(path.clone());
            if readable {
                self.readable.push(path);
            }
            self
        }
    }

    impl InputLayout for FakeLayout {
        fn event_devices(&self) -> Vec<PathBuf> {
            self.devices.clone()
        }

        fn readable(&self, device: &Path) -> bool {
            self.readable.iter().any(|candidate| candidate == device)
        }

        fn input_group(&self) -> GroupState {
            self.group
        }
    }

    #[test]
    fn a_readable_event_device_is_usable_with_no_message() {
        let layout = FakeLayout::new(GroupState::NotMember).device("/dev/input/event3", true);
        let access = assess(&layout);
        assert_eq!(access, PadAccess::Usable);
        assert_eq!(access.message(), None);
        assert!(!access.is_denied());
    }

    #[test]
    fn a_working_device_beside_an_unreadable_one_is_never_denied() {
        // The false positive the plan warns about: one readable node is enough.
        let layout = FakeLayout::new(GroupState::NotMember)
            .device("/dev/input/event0", true)
            .device("/dev/input/event1", false);
        assert_eq!(assess(&layout), PadAccess::Usable);
    }

    #[test]
    fn no_event_device_is_not_a_permission_problem() {
        let layout = FakeLayout::new(GroupState::Unknown);
        let access = assess(&layout);
        assert_eq!(access, PadAccess::NoDevices);
        assert_eq!(access.message(), None);
        assert!(!access.is_denied());
    }

    #[test]
    fn devices_present_but_unreadable_outside_the_group_asks_for_the_input_group() {
        let layout = FakeLayout::new(GroupState::NotMember).device("/dev/input/event2", false);
        let access = assess(&layout);
        assert_eq!(
            access,
            PadAccess::Denied {
                devices: 1,
                group: GroupState::NotMember,
            }
        );
        assert!(access.is_denied());
        let message = access.message().expect("a denial has a fix");
        assert!(message.contains("input group"));
        assert!(message.contains("usermod"));
    }

    #[test]
    fn devices_present_but_unreadable_in_the_group_points_at_uaccess() {
        let layout = FakeLayout::new(GroupState::Member).device("/dev/input/event2", false);
        let access = assess(&layout);
        let message = access.message().expect("a denial has a fix");
        assert!(message.contains("uaccess") || message.contains("replug"));
    }

    #[test]
    fn an_unknown_group_state_still_gives_one_actionable_line() {
        let layout = FakeLayout::new(GroupState::Unknown).device("/dev/input/event2", false);
        let message = assess(&layout).message().expect("a denial has a fix");
        assert!(message.contains("input group"));
    }

    #[test]
    fn the_real_layout_over_a_tempdir_finds_event_nodes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("event0"), b"").unwrap();
        std::fs::write(dir.path().join("mice"), b"").unwrap();
        let layout = DevInput::with_root(dir.path());
        assert_eq!(layout.event_devices(), vec![dir.path().join("event0")]);
        // A readable node means the pad path works.
        assert_eq!(assess(&layout), PadAccess::Usable);
    }

    #[test]
    fn the_real_layout_with_no_nodes_is_no_devices() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DevInput::with_root(dir.path());
        assert_eq!(assess(&layout), PadAccess::NoDevices);
    }

    #[test]
    fn a_probe_leaves_the_node_openable_by_a_second_reader() {
        // The probe's only contact with a node is a read-only `open`; it never
        // grabs, so a game can hold the same node while the launcher checks it.
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("event0");
        std::fs::write(&node, b"").unwrap();
        let layout = DevInput::with_root(dir.path());
        assert!(layout.readable(&node));
        // A second, independent reader still succeeds — nothing is exclusive.
        assert!(std::fs::File::open(&node).is_ok());
    }
}
