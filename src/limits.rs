//! Parser Claude Code usage-limit messages such as `You've hit your session limit · resets 2:40am (Europe/Rome)`.

use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

#[derive(Debug, Clone, PartialEq)]
pub struct LimitHit {
    /// "session", "weekly", "monthly spend", ... ("usage" when unknown)
    pub kind: String,
    pub reset_at: Option<DateTime<Utc>>,
}

/// Returns `Some` when `text` is a usage-limit-message. `seen_at` is the timestamp of the transcript line; the reset
/// is the first matching wall-clock time after it
pub fn parse_limit(text: &str, seen_at: DateTime<Utc>) -> Option<LimitHit> {
    let lower = text.to_lowercase();
    let start = lower.find("hit your ")? + "hit your ".len();
    let end = start + lower[start..].find(" limit")?;
    let kind = lower[start..end].trim();
    let kind = if kind.is_empty() { "usage" } else { kind }.to_string();
    Some(LimitHit {
        kind,
        reset_at: parse_reset(&lower, seen_at),
    })
}

fn parse_reset(lower: &str, seen_at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    // Take the last "resets", the one that names the limit that is actually blocking
    let after = &lower[lower.rfind("resets ")? + "resets ".len()..];
    let open = after.find('(')?;
    let close = open + after[open..].find(')')?;
    let when = after[..open].trim();
    // Time zome names are case-sensitive, so I read them from the original casing
    let tz: Tz = restore_tz_case(&after[open + 1..close]).parse().ok()?;

    let (date_part, time_part) = match when.rsplit_once(',') {
        Some((d, t)) => (Some(d.trim()), t.trim()),
        None => (None, when),
    };

    let time = parse_clock(time_part)?;
    let local_seen = seen_at.with_timezone(&tz);

    let date = match date_part {
        Some(d) => parse_monthy_day(d, local_seen.year())?,
        None => local_seen.date_naive(),
    };

    let mut reset = tz.from_local_datetime(&date.and_time(time)).earliest()?;
    if date_part.is_none() && reset <= local_seen {
        let next = date.checked_add_days(Days::new(1))?;
        reset = tz.from_local_datetime(&next.and_time(time)).earliest()?;
    }
    Some(reset.with_timezone(&Utc))
}

/// "4am", "2:40am", "12:40pm", "15:00"
fn parse_clock(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    let (digits, meridiem) = if let Some(d) = s.strip_suffix("am") {
        (d, Some(false))
    } else if let Some(d) = s.strip_suffix("pm") {
        (d, Some(true))
    } else {
        (s, None)
    };
    let (h, m) = match digits.trim().split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (digits.trim().parse::<u32>().ok()?, 0),
    };
    let h = match meridiem {
        Some(pm) if (1..=12).contains(&h) => (h % 12) + if pm { 12 } else { 0 },
        Some(_) => return None,
        None => h,
    };
    NaiveTime::from_hms_opt(h, m, 0)
}

/// "oct 1" / "sep 30"
fn parse_monthy_day(s: &str, year: i32) -> Option<NaiveDate> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let (mon, day) = s.split_once(' ')?;
    let month = MONTHS.iter().position(|m| mon.starts_with(m))? as u32 + 1;
    NaiveDate::from_ymd_opt(year, month, day.trim().parse().ok()?)
}

/// The message was lowercased for matching; IANA names need their casing back ("europe/rome" -> "Europe/Rome",
/// "america/new_york" -> "America/New_York").
fn restore_tz_case(tz: &str) -> String {
    let tz = tz.trim();
    if tz.eq_ignore_ascii_case("utc") {
        return "UTC".into();
    }
    tz.split('/')
        .map(|part| {
            part.split('_')
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join("_")
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn session_limit_later_same_day() {
        // 10:00 Rome (CEST, UTC+2) -> resets 12:40pm Rome = 10:40 UTC
        let hit = parse_limit(
            "You've hit your session limit · resets 12:40pm (Europe/Rome)",
            utc("2026-09-26T08:00:00Z"),
        )
        .unwrap();
        assert_eq!(hit.kind, "session");
        assert_eq!(hit.reset_at, Some(utc("2026-09-26T10:40:00Z")));
    }

    #[test]
    fn reset_rolls_over_to_next_day() {
        // 23:00 Rome -> "4am" means tomorrow 04:00 Rome = 02:00 UTC
        let hit = parse_limit(
            "You've hit your weekly limit · resets 4am (Europe/Rome)",
            utc("2026-09-26T21:00:00Z"),
        )
        .unwrap();
        assert_eq!(hit.kind, "weekly");
        assert_eq!(hit.reset_at, Some(utc("2026-09-27T02:00:00Z")));
    }

    #[test]
    fn monthly_spend_uses_last_reset() {
        let hit = parse_limit(
            "You've hit your monthly spend limit · raise it at claude.ai/settings/usage?from=cc_cli_limit_message · your session limit resets 6:40pm (Europe/Rome)",
            utc("2026-09-16T16:17:37Z"),
        )
        .unwrap();
        assert_eq!(hit.kind, "monthly spend");
        assert_eq!(hit.reset_at, Some(utc("2026-09-16T16:40:00Z")));
    }

    #[test]
    fn twenty_four_hour_clock() {
        let hit = parse_limit(
            "You've hit your session limit · resets 15:00 (Europe/Rome)",
            utc("2026-09-26T08:00:00Z"),
        )
        .unwrap();
        assert_eq!(hit.reset_at, Some(utc("2026-09-26T13:00:00Z")));
    }

    #[test]
    fn twelve_am_is_midnight() {
        let hit = parse_limit(
            "You've hit your session limit · resets 12am (UTC)",
            utc("2026-09-26T08:00:00Z"),
        )
        .unwrap();
        assert_eq!(hit.reset_at, Some(utc("2026-09-27T00:00:00Z")));
    }

    #[test]
    fn explicit_date() {
        let hit = parse_limit(
            "You've hit your weekly limit · resets Oct 1, 4am (America/New_York)",
            utc("2026-09-26T08:00:00Z"),
        )
        .unwrap();
        // 04:00 EDT (UTC-4) = 08:00 UTC.
        assert_eq!(hit.reset_at, Some(utc("2026-10-01T08:00:00Z")));
    }

    #[test]
    fn unknown_reset_format_keeps_the_hit() {
        let hit = parse_limit(
            "You've hit your session limit · resets soon",
            utc("2026-09-26T08:00:00Z"),
        )
        .unwrap();
        assert_eq!(hit.kind, "session");
        assert_eq!(hit.reset_at, None);
    }

    #[test]
    fn unknown_time_zone_keeps_the_hit() {
        let hit = parse_limit(
            "You've hit your session limit · resets 4am (Mars/Olympus)",
            utc("2026-09-26T08:00:00Z"),
        )
        .unwrap();
        assert_eq!(hit.reset_at, None);
    }

    #[test]
    fn other_text_is_not_a_limit() {
        assert_eq!(
            parse_limit("API Error: Connection lost mid-response.", utc("2026-09-26T08:00:00Z")),
            None
        );
    }
}
