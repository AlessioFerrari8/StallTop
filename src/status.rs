//! Derives a session's Status from the facts providers collect. Pure functions

use crate::limits::LimitHit;
use crate::model::Status;
use chrono::{DateTime, Utc};

/// Busy session with no transcript write for this long is shown as "quiet"
pub const QUIET_AFTER_SECS: u64 = 5 * 60;

/// Dead sessions stay on screen this long
pub const KEEP_DEAD_SECS: i64 = 5 * 60;

/// Gemini chat updated this recently, counts as working
pub const GEMINI_WORKING_SECS: i64 = 10;

// what claude writes in `sessions/<pid>.json`
#[derive(Debug, Clone, PartialEq)]
pub enum LiveState {
    Busy,
    Idle,
    Waiting { what: String, since: DateTime<Utc> },
}

pub struct ClaudeFacts<'a> {
    pub dead_since: Option<DateTime<Utc>>,
    pub live: &'a LiveState,
    // set when the last meaningful transcript line is a usage-limit error
    pub pending_limit: Option<&'a LimitHit>,
    pub last_write: Option<DateTime<Utc>>,
}

pub fn claude_status(f: &ClaudeFacts, now: DateTime<Utc>) -> Status {
    if let Some(since) = f.dead_since {
        return Status::Dead { since };
    }
    if let LiveState::Waiting { what, since } = f.live {
        return Status::NeedsYou {
            what: what.clone(),
            since: *since,
        };
    }
    if let Some(hit) = f.pending_limit {
        return Status::RateLimited {
            kind: hit.kind.clone(),
            reset_at: hit.reset_at,
        };
    }
    match f.live {
        LiveState::Busy => Status::Working {
            quiet_secs: secs_since(f.last_write, now),
        },
        _ => Status::Idle,
    }
}

pub fn gemini_status(
    dead_since: Option<DateTime<Utc>>,
    last_write: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Status {
    if let Some(since) = dead_since {
        return Status::Dead { since };
    }
    match last_write {
        Some(t) if (now - t).num_seconds() <= GEMINI_WORKING_SECS => Status::Working { quiet_secs: 0 },
        _ => Status::Idle,
    }
}

/// Codex: like Gemini, plus the limit read from its last rate-limit snapshot
pub fn codex_status(
    dead_since: Option<DateTime<Utc>>,
    limit: Option<&LimitHit>,
    last_write: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Status {
    match (dead_since, limit) {
        (None, Some(hit)) => Status::RateLimited {
            kind: hit.kind.clone(),
            reset_at: hit.reset_at,
        },
        _ => gemini_status(dead_since, last_write, now),
    }
}

/// True while a dead session should still be listed
pub fn keep_dead(since: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    (now - since).num_seconds() < KEEP_DEAD_SECS
}

fn secs_since(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> u64 {
    t.map(|t| (now - t).num_seconds().max(0) as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn facts<'a>(live: &'a LiveState, limit: Option<&'a LimitHit>) -> ClaudeFacts<'a> {
        ClaudeFacts {
            dead_since: None,
            live,
            pending_limit: limit,
            last_write: Some(utc("2026-09-26T12:00:00Z")),
        }
    }

    #[test]
    fn dead_beats_everything() {
        let live = LiveState::Waiting {
            what: "x".into(),
            since: utc("2026-09-26T12:00:00Z"),
        };
        let f = ClaudeFacts {
            dead_since: Some(utc("2026-09-26T12:01:00Z")),
            ..facts(&live, None)
        };
        assert!(matches!(
            claude_status(&f, utc("2026-09-26T12:02:00Z")),
            Status::Dead { .. }
        ));
    }

    #[test]
    fn waiting_beats_limit() {
        let live = LiveState::Waiting {
            what: "Bash(npm test)".into(),
            since: utc("2026-09-26T12:00:00Z"),
        };
        let hit = LimitHit {
            kind: "session".into(),
            reset_at: None,
        };
        let s = claude_status(&facts(&live, Some(&hit)), utc("2026-09-26T12:02:00Z"));
        assert_eq!(
            s,
            Status::NeedsYou {
                what: "Bash(npm test)".into(),
                since: utc("2026-09-26T12:00:00Z")
            }
        );
    }

    #[test]
    fn limit_beats_idle() {
        let hit = LimitHit {
            kind: "weekly".into(),
            reset_at: Some(utc("2026-09-27T02:00:00Z")),
        };
        let s = claude_status(&facts(&LiveState::Idle, Some(&hit)), utc("2026-09-26T12:02:00Z"));
        assert_eq!(
            s,
            Status::RateLimited {
                kind: "weekly".into(),
                reset_at: Some(utc("2026-09-27T02:00:00Z"))
            }
        );
    }

    #[test]
    fn busy_reports_quiet_time() {
        let s = claude_status(&facts(&LiveState::Busy, None), utc("2026-09-26T12:07:00Z"));
        assert_eq!(s, Status::Working { quiet_secs: 420 });
        assert_eq!(s.label(), "quiet");
        assert!(s.is_quiet());
        let s = claude_status(&facts(&LiveState::Busy, None), utc("2026-09-26T12:01:00Z"));
        assert_eq!(s.label(), "working");
    }

    #[test]
    fn gemini_working_window() {
        let w = Some(utc("2026-09-26T12:00:00Z"));
        assert_eq!(
            gemini_status(None, w, utc("2026-09-26T12:00:08Z")),
            Status::Working { quiet_secs: 0 }
        );
        assert_eq!(gemini_status(None, w, utc("2026-09-26T12:00:30Z")), Status::Idle);
    }

    #[test]
    fn dead_rows_expire() {
        let since = utc("2026-09-26T12:00:00Z");
        assert!(keep_dead(since, utc("2026-09-26T12:04:59Z")));
        assert!(!keep_dead(since, utc("2026-09-26T12:05:00Z")));
    }
}
