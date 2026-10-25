//! Number and time formatting shared by the TUI and the `--once` table

use crate::model::Status;
use chrono::{DateTime, Local, Utc};

/// 950 -> "950", 41_234 -> "41k", 1_930_000 -> "1.9M"
pub fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub fn cost(c: Option<f64>) -> String {
    c.map(|c| format!("${c:.2}")).unwrap_or_else(|| "?".into())
}

/// Compact age: "45s", "38m", "1h2m", "2d3h"
pub fn age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m", s / 60),
        3_600..=86_399 => format!("{}h{}m", s / 3_600, s % 3_600 / 60),
        _ => format!("{}d{}h", s / 86_400, s % 86_400 / 3_600),
    }
}

/// Waiting time with seconds while under an hour: "2m14s"
pub fn wait(secs: i64) -> String {
    let s = secs.max(0);
    if s < 3_600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        age(s)
    }
}

/// "claude-opus-5-5" -> "opus-5-5".
/// Model id without what every row would repeat: the `claude-` prefix, a date snapshot suffix (`-20250929`) and `-preview`
pub fn model(m: Option<&str>) -> String {
    let Some(m) = m else { return "—".into() };
    let m = m.strip_prefix("claude-").unwrap_or(m);
    let m = match m.rsplit_once('-') {
        Some((head, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => m,
    };
    m.strip_suffix("-preview").unwrap_or(m).to_string()
}

/// Text for the NEEDS YOU panel and the detail header
pub fn status_detail(status: &Status, now: DateTime<Utc>) -> String {
    match status {
        Status::NeedsYou { what, since } => format!("{what} · waiting {}", wait((now - *since).num_seconds())),
        Status::RateLimited {
            kind,
            reset_at: Some(r),
        } if *r > now => format!(
            "{kind} limit · resets {} (in {})",
            r.with_timezone(&Local).format("%H:%M"),
            age((*r - now).num_seconds())
        ),
        Status::RateLimited {
            kind,
            reset_at: Some(_),
        } => format!("{kind} limit · reset passed, can resume"),
        Status::RateLimited { kind, reset_at: None } => format!("{kind} limit · reset time unknown"),
        Status::Working { quiet_secs } if status.is_quiet() => {
            format!("working · quiet for {}", age(*quiet_secs as i64))
        }
        Status::Working { .. } => "working".into(),
        Status::Idle => "idle".into(),
        Status::Dead { since } => format!("ended {} ago", age((now - *since).num_seconds())),
    }
}

// tests
#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn numbers() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(412_345), "412k");
        assert_eq!(tokens(1_930_000), "1.9M");
        assert_eq!(cost(Some(1.834)), "$1.83");
        assert_eq!(cost(None), "?");
    }

    #[test]
    fn durations() {
        assert_eq!(age(45), "45s");
        assert_eq!(age(38 * 60), "38m");
        assert_eq!(age(3_720), "1h2m");
        assert_eq!(age(2 * 86_400 + 3 * 3_600), "2d3h");
        assert_eq!(wait(134), "2m14s");
        assert_eq!(age(-5), "0s");
    }

    #[test]
    fn details() {
        let now = utc("2026-09-26T12:00:00Z");
        let s = Status::RateLimited {
            kind: "session".into(),
            reset_at: Some(utc("2026-09-26T13:12:00Z")),
        };
        assert!(status_detail(&s, now).ends_with("(in 1h12m)"));
        let s = Status::RateLimited {
            kind: "session".into(),
            reset_at: Some(utc("2026-09-26T11:00:00Z")),
        };
        assert_eq!(status_detail(&s, now), "session limit · reset passed, can resume");
        let s = Status::NeedsYou {
            what: "permission: Bash".into(),
            since: utc("2026-09-26T11:57:46Z"),
        };
        assert_eq!(status_detail(&s, now), "permission: Bash · waiting 2m14s");
        assert_eq!(model(Some("claude-opus-5-5")), "opus-5-5");
        assert_eq!(model(Some("gemini-2.5-pro")), "gemini-2.5-pro");
        assert_eq!(model(Some("claude-sonnet-4-5-20250929")), "sonnet-4-5");
        assert_eq!(model(Some("gemini-3.1-pro-preview")), "gemini-3.1-pro");
        assert_eq!(model(None), "—");
    }
}
