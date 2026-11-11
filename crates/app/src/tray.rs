//! The tray icon.
//!
//! PitaGames is a launcher that can be behind a game, so it keeps a tray icon
//! with the two actions that matter: bring the window back, and quit.
//!
//! Dioxus wires the event *channels* itself (`tray_icon::TrayIconEvent` and
//! `tray_icon::menu::MenuEvent` are forwarded into the event loop), but it does
//! not create the icon: [`init_tray_icon`](dioxus::desktop::trayicon::init_tray_icon)
//! panics when the tray cannot be built. A launcher must not die because a
//! status-notifier host is missing, so the icon is built here and a failure is a
//! warning — the window simply works without a tray.
//!
//! A left click is handled by Dioxus itself (`tray_icon_show_window_on_click`,
//! on by default): it makes the window visible and focuses it.

use dioxus::desktop::trayicon::TrayIconBuilder;
use dioxus::desktop::trayicon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};

use crate::icon;

/// A live tray icon and the menu items the app reacts to.
///
/// Keep this value alive for as long as the window exists: dropping it removes
/// the icon from the tray.
pub struct Tray {
    // Fields drop in declaration order: the icon (and its menu view) goes
    // first, then our own menu handle.
    _icon: dioxus::desktop::trayicon::TrayIcon,
    // Keeping our own handle means the items cannot be collected while the
    // icon is alive.
    _menu: Menu,
    show: MenuId,
    quit: MenuId,
}

impl Tray {
    /// The id of "Show PitaGames".
    pub fn show_id(&self) -> &MenuId {
        &self.show
    }

    /// The id of "Quit PitaGames".
    pub fn quit_id(&self) -> &MenuId {
        &self.quit
    }
}

/// Build the tray icon, or `None` (with a warning) when the platform refuses.
///
/// On Linux this needs a StatusNotifierItem host (KDE, Cinnamon, GNOME with an
/// extension, …). Without one the build fails; that is not an error worth a
/// dialog, and the launcher is fully usable from its window and the menu entry.
///
/// The `libappindicator` wrapper panics (rather than returning an error) when
/// neither `libayatana-appindicator3` nor `libappindicator3` can be loaded, so
/// the attempt runs with the panic hook muted and inside `catch_unwind`. Any
/// panic here is a missing tray, not a broken launcher, and catching it keeps
/// the window on screen.
pub fn build_optional() -> Option<Tray> {
    // Mute the hook only for this call: a caught panic would otherwise print a
    // scary "thread 'main' panicked" trace for a launcher that is perfectly fine
    // without a tray. The window has not started rendering yet, so no other
    // thread can lose its panic message in this window.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
    std::panic::set_hook(hook);

    match outcome {
        Ok(Ok(tray)) => Some(tray),
        Ok(Err(error)) => {
            eprintln!("[pitagames] no tray icon: {error}");
            None
        }
        Err(_) => {
            eprintln!(
                "[pitagames] no tray icon: no status-notifier host (install \
                 libayatana-appindicator or libappindicator); the window still works"
            );
            None
        }
    }
}

fn build() -> Result<Tray, String> {
    let menu = Menu::new();
    let show = MenuItem::new("Show PitaGames", true, None);
    let quit = MenuItem::new("Quit PitaGames", true, None);
    menu.append_items(&[&show, &PredefinedMenuItem::separator(), &quit])
        .map_err(|error| error.to_string())?;

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip("PitaGames")
        // A left click raises the window (Dioxus's own handler); the menu is
        // for the right click.
        .with_menu_on_left_click(false);
    if let Some(icon) = icon::tray_icon(icon::DEFAULT_SIZE) {
        builder = builder.with_icon(icon);
    }

    let tray = builder.build().map_err(|error| error.to_string())?;

    Ok(Tray {
        _icon: tray,
        _menu: menu,
        show: show.id().clone(),
        quit: quit.id().clone(),
    })
}
