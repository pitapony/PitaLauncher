//! Small display formatters shared by the views.
//!
//! Framework-free and unit-tested: the grid, the downloads list and the CLI
//! style output all speak the same units.

use std::time::Duration;

use pitagames_core::i18n::t;
use time::{Month, OffsetDateTime};

/// Human-readable binary size (`12.0 GiB`).
pub fn human_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < 4 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", byte_unit(0))
    } else {
        format!("{value:.1} {}", byte_unit(unit))
    }
}

/// The size unit at `unit` (0 = bytes).
fn byte_unit(unit: usize) -> &'static str {
    match unit {
        0 => t("format.bytes_b"),
        1 => t("format.kib"),
        2 => t("format.mib"),
        3 => t("format.gib"),
        _ => t("format.tib"),
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
        return t("format.under_minute").to_string();
    }
    if minutes < 60 {
        return t("format.minutes").replace("{value}", &minutes.to_string());
    }

    let hours = minutes as f64 / 60.0;
    if hours < 100.0 {
        t("format.hours").replace("{value}", &format!("{hours:.1}"))
    } else {
        t("format.hours").replace("{value}", &format!("{hours:.0}"))
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
        return t("format.last_today").to_string();
    }
    if days == 1 {
        return t("format.last_yesterday").to_string();
    }
    if days < 7 {
        return t("format.days_ago").replace("{count}", &days.to_string());
    }

    let month = month_short(at.month());
    if at.year() == now.year() {
        t("format.date_day_month")
            .replace("{day}", &at.day().to_string())
            .replace("{month}", month)
    } else {
        t("format.date_day_month_year")
            .replace("{day}", &at.day().to_string())
            .replace("{month}", month)
            .replace("{year}", &at.year().to_string())
    }
}

/// A compact "how long ago" for a cache timestamp: `"just now"`, `"4 h ago"`.
///
/// Used by the account view for the owned-list refresh; the detail page's
/// `last_played` is calendar-shaped and stays separate.
pub fn duration_ago(age: time::Duration) -> String {
    let seconds = age.whole_seconds().max(0);
    if seconds < 60 {
        return t("format.just_now").to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return t("format.minutes_ago").replace("{count}", &minutes.to_string());
    }
    let hours = minutes / 60;
    if hours < 24 {
        return t("format.hours_ago").replace("{count}", &hours.to_string());
    }
    t("format.days_ago_short").replace("{count}", &(hours / 24).to_string())
}

/// A compact timestamp for the log list: `"today 14:32"` or `"09 Nov 14:32"`.
pub fn log_time(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let time = format!("{:02}:{:02}", at.hour(), at.minute());
    if at.date() == now.date() {
        return t("format.log_today").replace("{time}", &time);
    }
    t("format.date_day_month_time")
        .replace("{day}", &at.day().to_string())
        .replace("{month}", month_short(at.month()))
        .replace("{time}", &time)
}

fn month_short(month: Month) -> &'static str {
    match month {
        Month::January => t("format.month_jan"),
        Month::February => t("format.month_feb"),
        Month::March => t("format.month_mar"),
        Month::April => t("format.month_apr"),
        Month::May => t("format.month_may"),
        Month::June => t("format.month_jun"),
        Month::July => t("format.month_jul"),
        Month::August => t("format.month_aug"),
        Month::September => t("format.month_sep"),
        Month::October => t("format.month_oct"),
        Month::November => t("format.month_nov"),
        Month::December => t("format.month_dec"),
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

    #[test]
    fn log_time_is_relative_for_today_and_dated_otherwise() {
        let at = |year, month, day, hour, minute| {
            time::Date::from_calendar_date(year, month, day)
                .unwrap()
                .with_hms(hour, minute, 0)
                .unwrap()
                .assume_utc()
        };
        let now = at(2026, Month::November, 9, 18, 0);
        assert_eq!(
            log_time(at(2026, Month::November, 9, 14, 32), now),
            "today 14:32"
        );
        assert_eq!(
            log_time(at(2026, Month::November, 8, 9, 5), now),
            "8 Nov 09:05"
        );
    }
}
