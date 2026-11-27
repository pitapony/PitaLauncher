//! Power actions over logind/D-Bus.
//!
//! The console session (v1.1) is the one place a couch user controls the
//! machine, so the overlay's power menu — suspend, restart, shut down, and end
//! the session — needs a way to ask the host. It asks **logind** on the system
//! bus (`org.freedesktop.login1`), the same way `systemctl` and `loginctl` do,
//! and never spawns a shell: no `loginctl`, no `systemctl`, no `sh`.
//!
//! Authorization is logind's business, not ours. Every method is polkit-gated,
//! and the requests here pass `interactive = true` so polkit can ask the user to
//! authorize instead of refusing outright. The launcher never runs privileged
//! code and never touches `/proc` or the kernel directly.
//!
//! The seam is the [`PowerBackend`] trait: the real implementation
//! ([`logind::Logind`], Linux-only) speaks D-Bus, and every test uses a fake, so
//! the whole module is exercised without a bus and builds on a host with no
//! logind at all.

/// One power action the overlay can ask for.
///
/// The plan's power menu is suspend / restart / shut down; `TerminateSession` is
/// the session's own `loginctl terminate-session`, the action behind
/// exit-to-desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// Suspend the machine to RAM.
    Suspend,
    /// Restart the machine.
    Restart,
    /// Power the machine off.
    Shutdown,
    /// End the caller's session.
    TerminateSession,
}

impl PowerAction {
    /// Every action this module can ask for.
    pub const ALL: [PowerAction; 4] = [
        Self::Suspend,
        Self::Restart,
        Self::Shutdown,
        Self::TerminateSession,
    ];

    /// The actions the console overlay offers as the machine-power menu, in
    /// order.
    ///
    /// [`TerminateSession`](Self::TerminateSession) is the session's own
    /// exit-to-desktop action (`docs/v1.1-session.md`): the overlay adds it as a
    /// separate row **only in the console session**, so it is deliberately not
    /// part of this always-visible menu.
    pub const MENU: [PowerAction; 3] = [Self::Suspend, Self::Restart, Self::Shutdown];

    /// A short human label, for a message or a log line.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Suspend => "suspend",
            Self::Restart => "restart",
            Self::Shutdown => "shut down",
            Self::TerminateSession => "end the session",
        }
    }
}

impl std::fmt::Display for PowerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Why a power action did not happen.
#[derive(Debug, thiserror::Error)]
pub enum PowerError {
    /// logind refused the request: polkit denied it, the call failed, or the
    /// machine is already doing it.
    #[error("logind refused to {action}: {message}")]
    Refused {
        action: PowerAction,
        message: String,
    },
    /// The system bus or logind could not be reached.
    #[error("logind is not available: {message}")]
    Unavailable { message: String },
    /// This platform has no logind (only reachable off Linux).
    #[error("power actions need logind, which this platform does not have")]
    Unsupported,
}

impl PowerError {
    /// Wrap a D-Bus/zbus error as an unavailable logind, with its message.
    #[cfg(target_os = "linux")]
    fn unavailable(error: &zbus::Error) -> Self {
        Self::Unavailable {
            message: error.to_string(),
        }
    }
}

/// The boundary a power action crosses: a fake in tests, logind in the app.
///
/// One method, because every action is the same shape — ask the host, learn
/// whether it accepted. Implementations must not block forever or spawn a
/// process; the logind backend's D-Bus call is the only work.
pub trait PowerBackend: Send + Sync {
    /// Ask the host to perform `action`.
    fn request(&self, action: PowerAction) -> Result<(), PowerError>;
}

/// Ask the host to perform `action`, through the real backend.
///
/// On Linux this connects to the system bus and calls logind; on any other
/// platform there is no logind, so it reports [`PowerError::Unsupported`]. The
/// GUI only ships on Linux, but a total function keeps the app free of a
/// platform branch.
#[cfg(target_os = "linux")]
pub fn request(action: PowerAction) -> Result<(), PowerError> {
    logind::Logind::connect()?.request(action)
}

/// Ask the host to perform `action` (no logind on this platform).
#[cfg(not(target_os = "linux"))]
pub fn request(_action: PowerAction) -> Result<(), PowerError> {
    Err(PowerError::Unsupported)
}

/// The real backend: `org.freedesktop.login1` on the system bus.
///
/// Linux-only, and the only place in the workspace that speaks to logind. Every
/// method it calls is polkit-gated; it passes `interactive = true` so polkit may
/// prompt, and it never shells out.
#[cfg(target_os = "linux")]
pub mod logind {
    use super::{PowerAction, PowerBackend, PowerError};
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::OwnedObjectPath;

    const LOGIND: &str = "org.freedesktop.login1";
    const MANAGER_PATH: &str = "/org/freedesktop/login1";
    const MANAGER: &str = "org.freedesktop.login1.Manager";
    const SESSION: &str = "org.freedesktop.login1.Session";

    /// A connection to logind.
    pub struct Logind {
        connection: Connection,
    }

    impl Logind {
        /// Connect to the system bus.
        pub fn connect() -> Result<Self, PowerError> {
            let connection =
                Connection::system().map_err(|error| PowerError::unavailable(&error))?;
            Ok(Self { connection })
        }

        /// The `org.freedesktop.login1.Manager` proxy.
        fn manager(&self) -> Result<Proxy<'_>, PowerError> {
            Proxy::new(&self.connection, LOGIND, MANAGER_PATH, MANAGER)
                .map_err(|error| PowerError::unavailable(&error))
        }

        /// The caller's own session id, so `TerminateSession` names it.
        ///
        /// `GetSessionByPID` is explicit where the `session/self` alias would be
        /// a guess, and the session object's `Id` property is the name
        /// `TerminateSession` takes.
        fn session_id(&self) -> Result<String, PowerError> {
            let manager = self.manager()?;
            let path: OwnedObjectPath = manager
                .call("GetSessionByPID", &(std::process::id(),))
                .map_err(|error| PowerError::unavailable(&error))?;
            let session = Proxy::new(&self.connection, LOGIND, path.as_str(), SESSION)
                .map_err(|error| PowerError::unavailable(&error))?;
            session
                .get_property("Id")
                .map_err(|error| PowerError::unavailable(&error))
        }
    }

    impl PowerBackend for Logind {
        fn request(&self, action: PowerAction) -> Result<(), PowerError> {
            let manager = self.manager()?;
            // `interactive = true` lets polkit prompt rather than refuse; the
            // method itself is polkit-gated, so this process needs no privilege.
            let result: Result<(), zbus::Error> = match action {
                PowerAction::Suspend => manager.call("Suspend", &(true,)),
                PowerAction::Restart => manager.call("Reboot", &(true,)),
                PowerAction::Shutdown => manager.call("PowerOff", &(true,)),
                PowerAction::TerminateSession => {
                    let id = self.session_id()?;
                    manager.call("TerminateSession", &(id.as_str(),))
                }
            };
            result.map_err(|error| PowerError::Refused {
                action,
                message: error.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A backend that records what it was asked and can be told to refuse.
    #[derive(Default)]
    struct FakePower {
        requested: Mutex<Vec<PowerAction>>,
        refuse: Option<String>,
    }

    impl FakePower {
        fn refusing(message: &str) -> Self {
            Self {
                requested: Mutex::new(Vec::new()),
                refuse: Some(message.to_string()),
            }
        }

        fn requested(&self) -> Vec<PowerAction> {
            self.requested.lock().unwrap().clone()
        }
    }

    impl PowerBackend for FakePower {
        fn request(&self, action: PowerAction) -> Result<(), PowerError> {
            self.requested.lock().unwrap().push(action);
            match &self.refuse {
                Some(message) => Err(PowerError::Refused {
                    action,
                    message: message.clone(),
                }),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn every_action_is_listed_once() {
        assert_eq!(
            PowerAction::ALL,
            [
                PowerAction::Suspend,
                PowerAction::Restart,
                PowerAction::Shutdown,
                PowerAction::TerminateSession,
            ]
        );
    }

    #[test]
    fn the_menu_is_the_three_overlay_rows() {
        assert_eq!(
            PowerAction::MENU,
            [
                PowerAction::Suspend,
                PowerAction::Restart,
                PowerAction::Shutdown,
            ]
        );
        for action in PowerAction::MENU {
            assert!(PowerAction::ALL.contains(&action), "{action} is not in ALL");
        }
        assert!(
            !PowerAction::MENU.contains(&PowerAction::TerminateSession),
            "exit-to-desktop is a session row, not part of the machine-power menu"
        );
    }

    #[test]
    fn a_fake_backend_records_every_action() {
        let backend = FakePower::default();
        for action in PowerAction::ALL {
            backend.request(action).unwrap();
        }
        assert_eq!(backend.requested(), PowerAction::ALL.to_vec());
    }

    #[test]
    fn a_refusal_names_the_action_and_the_reason() {
        let backend = FakePower::refusing("Not authorized");
        let error = backend.request(PowerAction::Shutdown).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("shut down"), "{message}");
        assert!(message.contains("Not authorized"), "{message}");
    }

    #[test]
    fn power_never_spawns_a_process() {
        // The action is a D-Bus method call, not a process: a shell-out would be
        // a second privileged path with its own argument-injection surface, so
        // the ticket forbids it outright. There is no global way to forbid
        // `Command`, so this guard reads the module's own source and refuses a
        // process constructor (the doc comment's "no `loginctl`" prose is fine,
        // which is why the needles are code shapes, not command names).
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/power.rs");
        let text = std::fs::read_to_string(&source).expect("read power.rs");
        // Production code only: this test names the forbidden tokens to assert on.
        let text = text.split("#[cfg(test)]").next().unwrap_or(&text);
        for needle in ["Command::new", "process::Command", "sh -c"] {
            assert!(
                !text.contains(needle),
                "power must ask logind over D-Bus, not spawn a process ({needle})"
            );
        }
    }
}
