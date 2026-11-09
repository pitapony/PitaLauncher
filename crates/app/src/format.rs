//! Small display formatters shared by the views.
//!
//! Framework-free and unit-tested: the grid, the downloads list and the CLI
//! style output all speak the same units.

use std::time::Duration;

use time::{Month, OffsetDateTime};

/// Human-readable binary size (`12.0 GiB`).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Play time as the card shows it: `"12 min"`, `"3.5 h"`, `"184 h"`.
///
/// Anything under a minute is reported as `<1 min` so a launch never shows as
/// `0 min`.
pub fn playtime(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;

    if seconds < 60 {
        return "<1 min".to_string();
    }
    if minutes < 60 {
        return format!("{minutes} min");
    }

    let hours = minutes as f64 / 60.0;
    if hours < 100.0 {
        format!("{hours:.1} h")
    } else {
        format!("{hours:.0} h")
    }
}

/// Download percentage, or `None` when Steam has not written a total yet.
pub fn percent(downloaded: u64, total: u64) -> Option<u8> {
    if total == 0 {
        return None;
    }
    let fraction = (downloaded as f64 / total as f64).clamp(0.0, 1.0);
    Some((fraction * 100.0).round() as u8)
}

/// Last-played line: `"today"`, `"yesterday"`, `"3 days ago"`, `"12 Jun 2026"`.
///
/// Dates are shown in the caller's timezone; the label only depends on the
/// calendar distance, so a launch yesterday never reads as "today".
pub fn last_played(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let days = (now - at).whole_days();

    if days <= 0 {
        return "today".to_string();
    }
    if days == 1 {
        return "yesterday".to_string();
    }
    if days < 7 {
        return format!("{days} days ago");
    }

    let month = month_short(at.month());
    if at.year() == now.year() {
        format!("{} {month}", at.day())
    } else {
        format!("{} {month} {}", at.day(), at.year())
    }
}

/// A compact "how long ago" for a cache timestamp: `"just now"`, `"4 h ago"`.
///
/// Used by the account view for the owned-list refresh; the detail page's
/// `last_played` is calendar-shaped and stays separate.
pub fn duration_ago(age: time::Duration) -> String {
    let seconds = age.whole_seconds().max(0);
    if seconds < 60 {
        return "just now".to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} h ago");
    }
    format!("{} d ago", hours / 24)
}

fn month_short(month: Month) -> &'static str {
    match month {
        Month::January => "Jan",
        Month::February => "Feb",
        Month::March => "Mar",
        Month::April => "Apr",
        Month::May => "May",
        Month::June => "Jun",
        Month::July => "Jul",
        Month::August => "Aug",
        Month::September => "Sep",
        Month::October => "Oct",
        Month::November => "Nov",
        Month::December => "Dec",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_binary_and_keep_one_decimal() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(48_318_382_080), "45.0 GiB");
    }

    #[test]
    fn playtime_reads_like_a_game_card() {
        assert_eq!(playtime(Duration::ZERO), "<1 min");
        assert_eq!(playtime(Duration::from_secs(59)), "<1 min");
        assert_eq!(playtime(Duration::from_secs(60)), "1 min");
        assert_eq!(playtime(Duration::from_secs(45 * 60)), "45 min");
        assert_eq!(playtime(Duration::from_secs(90 * 60)), "1.5 h");
        assert_eq!(playtime(Duration::from_secs(100 * 3600)), "100 h");
    }

    #[test]
    fn percentages_are_clamped_and_undefined_without_a_total() {
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(1, 0), None);
        assert_eq!(percent(0, 100), Some(0));
        assert_eq!(percent(25, 100), Some(25));
        assert_eq!(percent(200, 100), Some(100));
    }

    #[test]
    fn last_played_reads_as_a_relative_date() {
        let date = |year, month, day| {
            time::Date::from_calendar_date(year, month, day)
                .unwrap()
                .with_hms(12, 0, 0)
                .unwrap()
                .assume_utc()
        };
        let now = date(2026, Month::June, 20);

        assert_eq!(last_played(now, now), "today");
        assert_eq!(last_played(now - time::Duration::days(1), now), "yesterday");
        assert_eq!(
            last_played(now - time::Duration::days(3), now),
            "3 days ago"
        );
        assert_eq!(
            last_played(date(2026, Month::May, 2), now),
            "2 May",
            "same year omits it"
        );
        assert_eq!(last_played(date(2024, Month::June, 12), now), "12 Jun 2024");
    }

    #[test]
    fn duration_ago_is_compact_and_never_negative() {
        use time::Duration as D;
        assert_eq!(duration_ago(-D::seconds(5)), "just now");
        assert_eq!(duration_ago(D::seconds(30)), "just now");
        assert_eq!(duration_ago(D::minutes(5)), "5 min ago");
        assert_eq!(duration_ago(D::hours(4)), "4 h ago");
        assert_eq!(duration_ago(D::hours(30)), "1 d ago");
    }
}
