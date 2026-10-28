//! Turns status changes into alerts, each alert fires once per transition

use crate::model::{Session, Status};
use chrono::{DateTime, Local, TimeDelta, Utc};
use std::collections::{HashMap, HashSet};

/// A limit cleared this close to its reset time counts as reset (clocks and polls drift)
const RESUME_GRACE_SECS: i64 = 5;

#[derive(Debug, Clone, PartialEq)]
pub enum Alert {
    NeedsYou {
        project: String,
        what: String,
    },
    Limited {
        project: String,
        kind: String,
        reset_at: Option<DateTime<Utc>>,
    },
    CanResume {
        project: String,
    },
}

impl Alert {
    /// (title, body) for a desktop notification
    pub fn message(&self) -> (String, String) {
        match self {
            Alert::NeedsYou { project, what } => (format!("{project} needs you"), what.clone()),
            Alert::Limited {
                project,
                kind,
                reset_at,
            } => {
                let when = reset_at
                    .map(|t| format!("resets {}", t.with_timezone(&Local).format("%H:%M")))
                    .unwrap_or_else(|| "reset time unknown".into());
                (format!("{project} hit a limit"), format!("{kind} limit · {when}"))
            }
            Alert::CanResume { project } => (format!("{project} can resume"), "The usage limit has reset".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    NeedsYou(String),
    Limited,
    Other,
}

#[derive(Default)]
pub struct Alerter {
    primed: bool,
    prev: HashMap<String, Seen>,
    /// Limited sessions whose reset is still in the future: key -> reset time
    waiting_reset: HashMap<String, DateTime<Utc>>,
}

impl Alerter {
    /// Call once per poll with every visible session. The first call only records state, so sessions already stuck when stalltop
    /// starts do not alert
    pub fn update(&mut self, sessions: &[Session], now: DateTime<Utc>) -> Vec<Alert> {
        let mut out = Vec::new();
        let mut present = HashSet::new();
        for s in sessions {
            let key = s.key();
            present.insert(key.clone());
            let seen = match &s.status {
                Status::NeedsYou { what, .. } => Seen::NeedsYou(what.clone()),
                Status::RateLimited { .. } => Seen::Limited,
                _ => Seen::Other,
            };
            let changed = self.prev.get(&key) != Some(&seen);
            if self.primed && changed {
                match &s.status {
                    Status::NeedsYou { what, .. } => out.push(Alert::NeedsYou {
                        project: s.project.clone(),
                        what: what.clone(),
                    }),
                    // A limit that already reset (for example an old transcript resumed) is not news
                    Status::RateLimited { kind, reset_at } if reset_at.is_none_or(|r| r > now) => {
                        out.push(Alert::Limited {
                            project: s.project.clone(),
                            kind: kind.clone(),
                            reset_at: *reset_at,
                        })
                    }
                    _ => {}
                }
            }
            match &s.status {
                Status::RateLimited { reset_at: Some(r), .. } if *r > now => {
                    self.waiting_reset.insert(key.clone(), *r);
                }
                Status::RateLimited { reset_at: Some(r), .. } => {
                    if self.waiting_reset.remove(&key) == Some(*r) {
                        out.push(Alert::CanResume {
                            project: s.project.clone(),
                        });
                    }
                }
                Status::RateLimited { reset_at: None, .. } | Status::Dead { .. } => {
                    self.waiting_reset.remove(&key);
                }
                // The time limit was cleared (the "Usage limit reset" line) before a poll saw the reset time pass: still
                // announce it when the reset time has come
                _ => {
                    if self
                        .waiting_reset
                        .remove(&key)
                        .is_some_and(|r| r <= now + TimeDelta::seconds(RESUME_GRACE_SECS))
                    {
                        out.push(Alert::CanResume {
                            project: s.project.clone(),
                        });
                    }
                }
            }
            self.prev.insert(key, seen);
        }
        self.prev.retain(|k, _| present.contains(k));
        self.waiting_reset.retain(|k, _| present.contains(k));
        self.primed = true;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Activity, Agent, Session, Status, into_session};
    use chrono::{DateTime, Utc};

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn session(id: &str, status: Status) -> Session {
        let t = utc("2026-09-26T12:00:00Z");
        into_session(
            Agent::Claude,
            id,
            Some(1),
            format!("/p/{id}").as_ref(),
            t,
            status,
            &Activity::default(),
            t,
        )
    }

    fn needs(what: &str) -> Status {
        Status::NeedsYou {
            what: what.into(),
            since: utc("2026-09-26T12:00:00Z"),
        }
    }

    fn limited(reset: &str) -> Status {
        Status::RateLimited {
            kind: "session".into(),
            reset_at: Some(utc(reset)),
        }
    }

    #[test]
    fn first_poll_is_silent() {
        let mut a = Alerter::default();
        assert!(
            a.update(&[session("a", needs("Bash"))], utc("2026-09-26T12:00:00Z"))
                .is_empty()
        );
    }

    #[test]
    fn needs_you_alerts_once() {
        let mut a = Alerter::default();
        let now = utc("2026-09-26T12:00:00Z");
        a.update(&[session("a", Status::Idle)], now);
        let alerts = a.update(&[session("a", needs("Bash"))], now);
        assert_eq!(
            alerts,
            vec![Alert::NeedsYou {
                project: "a".into(),
                what: "Bash".into()
            }]
        );
        assert!(a.update(&[session("a", needs("Bash"))], now).is_empty());
    }

    #[test]
    fn limit_alerts_then_can_resume() {
        let mut a = Alerter::default();
        let reset = "2026-09-26T13:00:00Z";
        a.update(&[session("a", Status::Idle)], utc("2026-09-26T12:00:00Z"));
        let alerts = a.update(&[session("a", limited(reset))], utc("2026-09-26T12:00:00Z"));
        assert!(matches!(alerts.as_slice(), [Alert::Limited { .. }]));
        let alerts = a.update(&[session("a", limited(reset))], utc("2026-09-26T13:00:01Z"));
        assert_eq!(alerts, vec![Alert::CanResume { project: "a".into() }]);
    }
}
