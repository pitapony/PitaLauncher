//! Design tokens for the two shells, and the contrast math behind them.
//!
//! The desktop shell and the 10-foot console shell share one component set, so
//! their differences are numbers, not markup: a bigger type scale, a bigger
//! minimum hit area and a darker backdrop. Those numbers live here — framework
//! free, so the floor and the contrast rules are testable without a webview.
//!
//! The CSS values are emitted as custom properties by the app's root style
//! (`app::shell::root_style`), so a component asks for `var(--text-caption)`
//! instead of hard-coding `12px` and the console shell gets 18 px from the same
//! markup. [`Rgb::contrast_ratio`] is the WCAG 2.1 relative-luminance ratio, so
//! a palette pair can be asserted against the 4.5:1 AA threshold in a unit test.

use crate::ui::{DisplayConfig, Shell};

/// The smallest text the console shell may draw, in CSS pixels.
///
/// The 10-foot rule: a caption read from a couch is still a caption, so the
/// floor applies after `ui_scale` (a busy tester cannot scale the console shell
/// down into unreadable type).
pub const CONSOLE_TYPE_FLOOR_PX: f32 = 18.0;
/// The smallest text the desktop shell may draw, in CSS pixels.
pub const DESKTOP_TYPE_FLOOR_PX: f32 = 11.0;
/// The smallest square a pointer or a touch may aim at in the console shell.
pub const CONSOLE_HIT_MIN_PX: f32 = 48.0;
/// The smallest square the desktop shell's controls keep.
pub const DESKTOP_HIT_MIN_PX: f32 = 28.0;
/// The focus ring's width, shared by both shells.
pub const FOCUS_RING_PX: f32 = 2.0;

/// A colour as the webview spells it, with the contrast math it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// A colour from its three channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#rrggbb` (the form the UI's inline styles use).
    pub fn from_hex(value: &str) -> Option<Self> {
        let hex = value.strip_prefix('#').unwrap_or(value);
        if hex.len() != 6 {
            return None;
        }
        let channel = |start: usize| u8::from_str_radix(&hex[start..start + 2], 16).ok();
        Some(Self::new(channel(0)?, channel(2)?, channel(4)?))
    }

    /// The `#rrggbb` spelling.
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// WCAG 2.1 relative luminance, 0.0 (black) to 1.0 (white).
    pub fn luminance(self) -> f32 {
        fn linear(channel: u8) -> f32 {
            let value = channel as f32 / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// The WCAG contrast ratio between two colours, 1.0 to 21.0.
    ///
    /// Symmetric; the order of the arguments never changes the answer.
    pub fn contrast_ratio(self, other: Self) -> f32 {
        let a = self.luminance();
        let b = other.luminance();
        let (lighter, darker) = if a >= b { (a, b) } else { (b, a) };
        (lighter + 0.05) / (darker + 0.05)
    }
}

/// The type roles a screen may ask for.
pub type TypeRole = &'static str;

/// The five text sizes the shells share.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeScale {
    /// Hero and game titles.
    pub title: f32,
    /// Panel and section headings.
    pub heading: f32,
    /// Normal copy, buttons and rows.
    pub body: f32,
    /// Secondary copy and metadata.
    pub caption: f32,
    /// Uppercase labels and badges.
    pub micro: f32,
}

impl TypeScale {
    /// Every role with its resolved size, in the order the CSS names them.
    pub fn roles(&self) -> [(&'static str, f32); 5] {
        [
            ("title", self.title),
            ("heading", self.heading),
            ("body", self.body),
            ("caption", self.caption),
            ("micro", self.micro),
        ]
    }

    /// The smallest resolved size, the one the floor is checked against.
    pub fn smallest(&self) -> f32 {
        self.roles()
            .iter()
            .map(|(_, px)| *px)
            .fold(f32::INFINITY, f32::min)
    }
}

/// The design tokens a shell renders with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DesignTokens {
    /// The resolved text sizes.
    pub type_scale: TypeScale,
    /// The minimum width and height of an interactive control.
    pub hit_min_px: f32,
    /// The focus ring's width.
    pub ring_px: f32,
}

impl DesignTokens {
    /// The type floor for a shell.
    pub const fn type_floor(shell: Shell) -> f32 {
        match shell {
            Shell::Desktop => DESKTOP_TYPE_FLOOR_PX,
            Shell::Console => CONSOLE_TYPE_FLOOR_PX,
        }
    }

    /// The hit-area minimum for a shell.
    pub const fn hit_floor(shell: Shell) -> f32 {
        match shell {
            Shell::Desktop => DESKTOP_HIT_MIN_PX,
            Shell::Console => CONSOLE_HIT_MIN_PX,
        }
    }

    /// Resolve the tokens for a shell and the display preferences.
    ///
    /// Every size is multiplied by `ui_scale` and then clamped to the shell's
    /// floor: scaling the console shell down must not drop a caption below the
    /// 10-foot floor.
    pub fn for_shell(shell: Shell, display: &DisplayConfig) -> Self {
        let base = match shell {
            Shell::Desktop => TypeScale {
                title: 28.0,
                heading: 16.0,
                body: 14.0,
                caption: 12.0,
                micro: 11.0,
            },
            Shell::Console => TypeScale {
                title: 40.0,
                heading: 26.0,
                body: 20.0,
                caption: 18.0,
                micro: 18.0,
            },
        };
        let floor = Self::type_floor(shell);
        let scale = display.ui_scale.max(0.0);
        let scaled = |px: f32| (px * scale).max(floor);
        Self {
            type_scale: TypeScale {
                title: scaled(base.title),
                heading: scaled(base.heading),
                body: scaled(base.body),
                caption: scaled(base.caption),
                micro: scaled(base.micro),
            },
            hit_min_px: Self::hit_floor(shell),
            ring_px: FOCUS_RING_PX,
        }
    }

    /// The token values as CSS custom properties, ready for a root style.
    pub fn css_vars(&self) -> String {
        let mut css = String::new();
        for (role, px) in self.type_scale.roles() {
            css.push_str(&format!("--text-{role}:{px}px;"));
        }
        css.push_str(&format!(
            "--hit-min:{}px;--ring:{}px;",
            self.hit_min_px, self.ring_px
        ));
        css
    }
}

/// The palette the UI's inline styles draw from.
///
/// The names are the roles; `Rgb::hex` turns them back into CSS. Changing a
/// value here changes every screen that uses the variable, and the contrast
/// test fails if a pair drops below AA.
pub const BACKGROUND: Rgb = Rgb::new(0x14, 0x14, 0x1c);
pub const CONSOLE_BACKGROUND: Rgb = Rgb::new(0x0b, 0x0b, 0x10);
pub const CARD: Rgb = Rgb::new(0x1e, 0x1e, 0x28);
pub const TEXT: Rgb = Rgb::new(0xe8, 0xe8, 0xee);
pub const TEXT_SECONDARY: Rgb = Rgb::new(0xc8, 0xc8, 0xd4);
/// The old muted grey (#78788a) failed AA on both backgrounds; this is the
/// nearest value that passes 4.5:1 on the card.
pub const TEXT_MUTED: Rgb = Rgb::new(0x9a, 0x9a, 0xac);
pub const TEXT_DISABLED: Rgb = Rgb::new(0x5a, 0x5a, 0x68);
pub const ACCENT: Rgb = Rgb::new(0x7a, 0x9a, 0xd6);
pub const WARNING: Rgb = Rgb::new(0xd8, 0xb0, 0x70);
pub const DANGER: Rgb = Rgb::new(0xe8, 0xa0, 0xa0);
pub const SUCCESS: Rgb = Rgb::new(0xb8, 0xdc, 0xc4);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{MAX_UI_SCALE, MIN_UI_SCALE};

    /// The text/background pairs the UI actually ships. Kept next to the
    /// palette so a colour change is checked against every surface at once.
    fn text_pairs() -> Vec<(&'static str, Rgb, Rgb)> {
        vec![
            ("primary on desktop background", TEXT, BACKGROUND),
            ("primary on console background", TEXT, CONSOLE_BACKGROUND),
            ("primary on a card", TEXT, CARD),
            ("secondary on a card", TEXT_SECONDARY, CARD),
            ("muted on desktop background", TEXT_MUTED, BACKGROUND),
            (
                "muted on console background",
                TEXT_MUTED,
                CONSOLE_BACKGROUND,
            ),
            ("muted on a card", TEXT_MUTED, CARD),
            ("accent on a card", ACCENT, CARD),
            ("warning on a card", WARNING, CARD),
            (
                "danger on the notice background",
                DANGER,
                Rgb::new(0x3a, 0x1e, 0x1e),
            ),
            (
                "success on the playing background",
                SUCCESS,
                Rgb::new(0x1e, 0x2a, 0x24),
            ),
        ]
    }

    #[test]
    fn contrast_matches_known_wcag_values() {
        // Independent worked examples, not values the implementation computes.
        let white = Rgb::new(255, 255, 255);
        let black = Rgb::new(0, 0, 0);
        assert!((black.contrast_ratio(white) - 21.0).abs() < 0.01);
        // #767676 on white is the canonical "just passes AA" grey (~4.54).
        let grey = Rgb::new(0x76, 0x76, 0x76);
        assert!((grey.contrast_ratio(white) - 4.54).abs() < 0.02);
        assert_eq!(grey.contrast_ratio(grey), 1.0);
        assert!((grey.contrast_ratio(white) - white.contrast_ratio(grey)).abs() < 1e-6);
    }

    #[test]
    fn every_shipped_text_pair_meets_aa() {
        for (name, foreground, background) in text_pairs() {
            let ratio = foreground.contrast_ratio(background);
            assert!(
                ratio >= 4.5,
                "{name}: {ratio:.2}:1 is below the 4.5:1 AA threshold"
            );
        }
    }

    #[test]
    fn the_console_type_never_falls_below_the_ten_foot_floor() {
        for scale in [MIN_UI_SCALE, 0.9, 1.0, 1.5, MAX_UI_SCALE] {
            let display = DisplayConfig {
                ui_scale: scale,
                ..DisplayConfig::default()
            };
            let tokens = DesignTokens::for_shell(Shell::Console, &display);
            let smallest = tokens.type_scale.smallest();
            assert!(
                smallest >= CONSOLE_TYPE_FLOOR_PX,
                "scale {scale}: smallest text is {smallest}px"
            );
        }
    }

    #[test]
    fn the_desktop_type_never_falls_below_its_own_floor() {
        for scale in [MIN_UI_SCALE, 1.0, MAX_UI_SCALE] {
            let display = DisplayConfig {
                ui_scale: scale,
                ..DisplayConfig::default()
            };
            let tokens = DesignTokens::for_shell(Shell::Desktop, &display);
            assert!(tokens.type_scale.smallest() >= DESKTOP_TYPE_FLOOR_PX);
        }
    }

    #[test]
    fn scaling_up_grows_a_size_that_is_above_the_floor() {
        let display = DisplayConfig {
            ui_scale: 1.5,
            ..DisplayConfig::default()
        };
        let tokens = DesignTokens::for_shell(Shell::Console, &display);
        assert!((tokens.type_scale.body - 30.0).abs() < 1e-6);
        assert!((tokens.type_scale.micro - 27.0).abs() < 1e-6);
    }

    #[test]
    fn the_console_shell_has_a_larger_hit_area_than_the_desktop_one() {
        let display = DisplayConfig::default();
        let desktop = DesignTokens::for_shell(Shell::Desktop, &display);
        let console = DesignTokens::for_shell(Shell::Console, &display);
        assert_eq!(desktop.hit_min_px, DESKTOP_HIT_MIN_PX);
        assert_eq!(console.hit_min_px, CONSOLE_HIT_MIN_PX);
        assert!(console.hit_min_px > desktop.hit_min_px);
        assert!(
            console.hit_min_px >= 44.0,
            "a 10-foot target is at least 44px"
        );
    }

    #[test]
    fn the_shells_name_the_same_css_variables() {
        let display = DisplayConfig::default();
        let desktop = DesignTokens::for_shell(Shell::Desktop, &display);
        let console = DesignTokens::for_shell(Shell::Console, &display);
        let mut names = Vec::new();
        for tokens in [desktop, console] {
            let css = tokens.css_vars();
            for role in ["title", "heading", "body", "caption", "micro"] {
                let name = format!("--text-{role}:");
                assert!(css.contains(&name), "{name} missing from {css}");
                names.push(name);
            }
            for name in ["--hit-min:", "--ring:"] {
                assert!(css.contains(name), "{name} missing from {css}");
                names.push(name.to_string());
            }
            assert!(css.ends_with(';'), "every declaration is terminated");
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 7, "exactly seven token variables");
    }

    #[test]
    fn the_palette_round_trips_through_hex() {
        for colour in [BACKGROUND, CARD, TEXT, ACCENT, WARNING, DANGER, SUCCESS] {
            let hex = colour.hex();
            assert_eq!(Rgb::from_hex(&hex), Some(colour));
        }
        assert_eq!(Rgb::from_hex("#000000"), Some(Rgb::new(0, 0, 0)));
        assert_eq!(Rgb::from_hex("000000"), Some(Rgb::new(0, 0, 0)));
        assert_eq!(Rgb::from_hex("#fff"), None);
        assert_eq!(Rgb::from_hex("#gggggg"), None);
    }
}
