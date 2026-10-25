//! Core data types shared by providers, status logic, alerts and the UI

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

/// How many events a session keeps for the detail view
pub const MAX_EVENTS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Gemini,
    Codex,
}

impl Agent {
    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Gemini => "gemini",
            Agent::Codex => "codex",
        }
    }
}

/// Token counters. Gemini "thoughts" are billed as output and are added there
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.cache_read += other.cache_read;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// Waiting on a human (permission prompt, question, dialog)
    NeedsYou {
        what: String,
        since: DateTime<Utc>,
    },
    /// Hit a usage limit. `reset_at` is None when the reset time could not be parsed
    RateLimited {
        kind: String,
        reset_at: Option<DateTime<Utc>>,
    },
    /// Busy. `quiet_secs` = seconds since the transcript last changed
    Working {
        quiet_secs: u64,
    },
    Idle,
    /// Process gone. Kept on screen for a few minutes
    Dead {
        since: DateTime<Utc>,
    },
}

impl Status {
    /// Busy, but nothing written to the transcript for `QUIET_AFTER_SECS`
    pub fn is_quiet(&self) -> bool {
        matches!(self, Status::Working { quiet_secs } if *quiet_secs >= crate::status::QUIET_AFTER_SECS)
    }

    /// Sort rank: lower comes first on screen
    pub fn rank(&self) -> u8 {
        match self {
            Status::NeedsYou { .. } => 0,
            Status::RateLimited { .. } => 1,
            Status::Working { .. } if self.is_quiet() => 2,
            Status::Working { .. } => 3,
            Status::Idle => 4,
            Status::Dead { .. } => 5,
        }
    }

    /// Short label for the STATUS column
    pub fn label(&self) -> &'static str {
        match self {
            Status::NeedsYou { .. } => "needs you",
            Status::RateLimited { .. } => "limited",
            Status::Working { .. } if self.is_quiet() => "quiet",
            Status::Working { .. } => "working",
            Status::Idle => "idle",
            Status::Dead { .. } => "ended",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Prompt,
    Tool,
    Reply,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub at: DateTime<Utc>,
    pub kind: EventKind,
    pub text: String,
}

/// Tokens and cost of one local calendar day
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct DayTotals {
    pub tokens: TokenUsage,
    pub cost_usd: f64,
    // Messages whose model has no known price (their cost is not in `cost_usd`)
    pub unpriced_messages: u64,
}

impl DayTotals {
    pub fn add(&mut self, other: &DayTotals) {
        self.tokens.add(&other.tokens);
        self.cost_usd += other.cost_usd;
        self.unpriced_messages += other.unpriced_messages;
    }
}

/// Everything a provider knows about one session, built from its transcript.
/// Providers fill it incrementally; `into_session` turns it into what the UI shows
#[derive(Debug, Clone, Default)]
pub struct Activity {
    pub title: Option<String>,
    pub model: Option<String>,
    pub tokens: TokenUsage,
    pub cost_usd: f64,
    pub unpriced_messages: u64,
    pub last_tool: Option<String>,
    pub tool_counts: BTreeMap<String, u32>,
    pub files_touched: BTreeSet<PathBuf>,
    pub events: VecDeque<Event>,
    pub first_activity: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
    // tokens x minute, keyed by unix minute
    pub token_minutes: BTreeMap<i64, u64>,
    // tokens and cost per local day
    pub days: BTreeMap<NaiveDate, DayTotals>,
}

impl Activity {
    pub fn push_event(&mut self, event: Event) {
        self.touch(event.at);
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    pub fn touch(&mut self, at: DateTime<Utc>) {
        if self.first_activity.is_none_or(|t| at < t) {
            self.first_activity = Some(at);
        }
        if self.last_activity.is_none_or(|t| at > t) {
            self.last_activity = Some(at);
        }
    }

    /// Records one billed message. `cost` is None when the model is not priced
    pub fn add_usage(&mut self, at: DateTime<Utc>, usage: &TokenUsage, cost: Option<f64>) {
        self.touch(at);
        self.tokens.add(usage);
        *self.token_minutes.entry(at.timestamp().div_euclid(60)).or_default() += usage.total();
        let day = self.days.entry(at.with_timezone(&Local).date_naive()).or_default();
        day.tokens.add(usage);
        match cost {
            Some(c) => {
                self.cost_usd += c;
                day.cost_usd += c;
            }
            None if usage.total() > 0 => {
                self.unpriced_messages += 1;
                day.unpriced_messages += 1;
            }
            None => {}
        }
    }

    /// Adds another activity's usage (tokens, cost, x-minute and x-day counters) and its time span
    /// Used to fold subagent transcripts into their parent session
    pub fn merge_usage(&mut self, other: &Activity) {
        for t in [other.first_activity, other.last_activity].into_iter().flatten() {
            self.touch(t);
        }
        self.tokens.add(&other.tokens);
        self.cost_usd += other.cost_usd;
        self.unpriced_messages += other.unpriced_messages;
        for (minute, n) in &other.token_minutes {
            *self.token_minutes.entry(*minute).or_default() += n;
        }
        for (date, d) in &other.days {
            self.days.entry(*date).or_default().add(d);
        }
    }

    pub fn day(&self, date: NaiveDate) -> DayTotals {
        self.days.get(&date).copied().unwrap_or_default()
    }
}

/// One row of the table
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Session {
    pub agent: Agent,
    pub id: String,
    pub pid: Option<u32>,
    pub project: String,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: Status,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub tokens: TokenUsage,
    // none when at least one message used a model with no known price
    pub cost_usd: Option<f64>,
    pub last_tool: Option<String>,
    pub tool_counts: BTreeMap<String, u32>,
    pub files_touched: BTreeSet<PathBuf>,
    pub recent_events: Vec<Event>,
    // tokens x minute for the last 60 minutes, oldest first
    pub tokens_per_minute: Vec<u64>,
}

impl Session {
    // stable identity across polls
    pub fn key(&self) -> String {
        format!("{}:{}", self.agent.label(), self.id)
    }
}

/// Last path component of a working directory, used as the project name
pub fn project_name(cwd: &std::path::Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string_lossy().into_owned())
}

/// Builds a Session from an Activity plus the facts only the provider knows
#[allow(clippy::too_many_arguments)]
pub fn into_session(
    agent: Agent,
    id: &str,
    pid: Option<u32>,
    cwd: &std::path::Path,
    started_at: DateTime<Utc>,
    status: Status,
    activity: &Activity,
    now: DateTime<Utc>,
) -> Session {
    let now_min = now.timestamp().div_euclid(60);
    let tokens_per_minute = (now_min - 59..=now_min)
        .map(|m| activity.token_minutes.get(&m).copied().unwrap_or(0))
        .collect();
    Session {
        agent,
        id: id.to_string(),
        pid,
        project: project_name(cwd),
        cwd: cwd.to_path_buf(),
        title: activity.title.clone(),
        model: activity.model.clone(),
        status,
        started_at,
        last_activity: activity.last_activity.unwrap_or(started_at),
        tokens: activity.tokens,
        cost_usd: (activity.unpriced_messages == 0).then_some(activity.cost_usd),
        last_tool: activity.last_tool.clone(),
        tool_counts: activity.tool_counts.clone(),
        files_touched: activity.files_touched.clone(),
        recent_events: activity.events.iter().cloned().collect(),
        tokens_per_minute,
    }
}

/// What `--once --json` prints and what the header shows
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub at: DateTime<Utc>,
    pub sessions: Vec<Session>,
    pub today: DayTotals,
    pub providers: Vec<ProviderState>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderState {
    pub name: &'static str,
    pub found: bool,
    pub skipped_lines: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 26, h, m, 0).unwrap()
    }

    #[test]
    fn events_are_capped() {
        let mut a = Activity::default();
        for i in 0..30 {
            a.push_event(Event {
                at: t(12, i),
                kind: EventKind::Tool,
                text: i.to_string(),
            });
        }
        assert_eq!(a.events.len(), MAX_EVENTS);
        assert_eq!(a.events.front().unwrap().text, "10");
        assert_eq!(a.first_activity, Some(t(12, 0)));
        assert_eq!(a.last_activity, Some(t(12, 29)));
    }

    #[test]
    fn unpriced_usage_makes_cost_unknown() {
        let mut a = Activity::default();
        let u = TokenUsage {
            input: 10,
            output: 5,
            ..Default::default()
        };

        a.add_usage(t(12, 0), &u, Some(1.0));
        a.add_usage(t(12, 1), &u, None);
        let s = into_session(
            Agent::Claude,
            "x",
            None,
            "/p/demo".as_ref(),
            t(11, 0),
            Status::Idle,
            &a,
            t(12, 2),
        );
        assert_eq!(s.cost_usd, None);
        assert_eq!(s.tokens.total(), 30);
        assert_eq!(s.project, "demo");
        assert_eq!(s.tokens_per_minute.len(), 60);
        assert_eq!(s.tokens_per_minute[57..], [15, 15, 0]);
    }

    #[test]
    fn zero_token_messages_do_not_count_as_unpriced() {
        let mut a = Activity::default();
        a.add_usage(t(12, 0), &TokenUsage::default(), None);
        assert_eq!(a.unpriced_messages, 0);
    }

    #[test]
    fn merge_usage_adds_counters_and_extends_the_span() {
        let u = TokenUsage {
            input: 10,
            ..Default::default()
        };
        let mut parent = Activity::default();
        parent.add_usage(t(12, 0), &u, Some(1.0));
        let mut sub = Activity::default();
        sub.add_usage(t(12, 0), &u, Some(0.5));
        sub.add_usage(t(12, 5), &u, None);
        sub.last_tool = Some("Bash ls".into());
        parent.merge_usage(&sub);
        assert_eq!(parent.tokens.input, 30);
        assert_eq!(parent.cost_usd, 1.5);
        assert_eq!(parent.unpriced_messages, 1);
        assert_eq!(parent.last_activity, Some(t(12, 5)));
        assert_eq!(parent.token_minutes.values().sum::<u64>(), 30);
        assert_eq!(parent.day(t(12, 0).with_timezone(&Local).date_naive()).tokens.input, 30);
        // Only usage is merged, not what the subagent did
        assert_eq!(parent.last_tool, None);
    }
}
