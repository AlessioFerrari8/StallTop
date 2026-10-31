//! Agent-specific data readers. Each provider turns local files into Sessions

pub mod claude;
pub mod codex;
pub mod gemini;

use crate::model::{DayTotals, ProviderState, Session, Snapshot};
use crate::procfs::ProcessTable;
use chrono::{DateTime, Local, NaiveDate, Utc};
use std::path::{Path, PathBuf};

/// Directories are rescanned for new transcripts every:
pub const RESCAN_EVERY: u64 = 30;

pub struct ProviderReport {
    pub sessions: Vec<Session>,
    pub today: DayTotals,
    pub state: ProviderState,
}

pub trait Provider {
    fn poll(&mut self, now: DateTime<Utc>, procs: &dyn ProcessTable) -> ProviderReport;
}

/// Where each agent keeps its data
pub struct Roots {
    pub claude: PathBuf,
    pub gemini: PathBuf,
    // pub codex: PathBuf,
}

impl Roots {
    /// `home` overrides the real home directory (tests, demos). Without it, CLAUDE_CONFIG_DIR and CODEX_HOME are honoured
    /// like the agents themselves do
    pub fn resolve(home: Option<&Path>) -> Self {
        match home {
            Some(h) => Self {
                claude: h.join(".claude"),
                gemini: h.join(".gemini"),
                // codex: h.join(".codex"),
            },
            None => {
                let h = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| h.join(".claude"));
                // let codex = std::env::var_os("CODEX_HOME")
                //     .map(PathBuf::from)
                //     .unwrap_or_else(|| h.join(".codex"));
                Self {
                    claude,
                    gemini: h.join(".gemini"),
                    // codex,
                }
            }
        }
    }
}

pub fn default_providers(roots: &Roots) -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(claude::ClaudeProvider::new(roots.claude.clone())),
        Box::new(gemini::GeminiProvider::new(roots.gemini.clone())),
        // Box::new(codex::CodexProvider::new(roots.codex.clone())),
    ]
}

/// Polls every provider once and merges the results
pub fn collect(providers: &mut [Box<dyn Provider>], procs: &dyn ProcessTable, now: DateTime<Utc>) -> Snapshot {
    let mut sessions = Vec::new();
    let mut today = DayTotals::default();
    let mut states = Vec::new();

    for p in providers.iter_mut() {
        let r = p.poll(now, procs);
        sessions.extend(r.sessions);
        today.add(&r.today);
        states.push(r.state);
    }
    sessions.sort_by(|a, b| {
        a.status
            .rank()
            .cmp(&b.status.rank())
            .then(b.last_activity.cmp(&a.last_activity))
    });
    Snapshot {
        at: now,
        sessions,
        today,
        providers: states,
    }
}

pub fn local_day(now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&Local).date_naive()
}

/// True when the file was modified on the local calendar day of `now`
pub fn modified_on(path: &Path, day: NaiveDate) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| DateTime::<Local>::from(t).date_naive() == day)
        .unwrap_or(false)
}

pub fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

/// One-line, length-limited version of a text for tables and event lists
pub fn clip(s: &str, max: usize) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_flattens_and_cuts() {
        assert_eq!(clip("a\n b", 10), "a b");
        assert_eq!(clip("abcdefghij", 5), "abcd...");
    }

    #[test]
    fn home_override() {
        let r = Roots::resolve(Some(Path::new("/tmp/h")));
        assert_eq!(r.claude, Path::new("/tmp/h/.claude"));
    }
}
