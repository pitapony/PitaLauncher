//! The launcher's session model: the shell's state behind one handle.
//!
//! Framework-free (ADR-0027). Right now it owns the [`Navigation`]; the
//! transient `notice` line, the shared concern handles (`Play`, `Installs`,
//! `Downloads`) and the single `dispatch(Action)` join it in the next slice.
//! It `Deref`s to [`Navigation`] so a screen writes `session.read().view()` and
//! never has to name the inner model.

use std::ops::{Deref, DerefMut};

use crate::navigation::{Navigation, View};

/// The shell's state: where the window is and what it has open.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Session {
    navigation: Navigation,
}

impl Session {
    /// A fresh session on `view`.
    pub(crate) fn new(view: View) -> Self {
        Self {
            navigation: Navigation::new(view),
        }
    }
}

impl Deref for Session {
    type Target = Navigation;

    fn deref(&self) -> &Navigation {
        &self.navigation
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Navigation {
        &mut self.navigation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_delegates_navigation_through_the_one_handle() {
        let mut session = Session::new(View::Home);
        // `Deref` reaches the navigation model without naming it.
        assert_eq!(session.view(), View::Home);
        session.set_view(View::Logs);
        assert_eq!(session.view(), View::Logs);
        session.go(View::Stores);
        assert_eq!(session.view(), View::Stores);
        assert_eq!(session.back(), View::Library);
    }
}
