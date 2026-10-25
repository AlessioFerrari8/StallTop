//! UI state and key handling. No I/O: main.rs, owns the terminal and the loop

use crate::model::{Session, Snapshot};
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    None,
    Quit,
}

pub struct App {
    pub snapshot: Snapshot,
    pub selected: usize,
    pub notify: bool,
    /// Last alert text, shown in the footer for a while
    pub flash: Option<(String, DateTime<Utc>)>,
}

impl App {
    pub fn new(notify: bool, now: DateTime<Utc>) -> Self {
        Self {
            snapshot: Snapshot {
                at: now,
                sessions: Vec::new(),
                today: Default::default(),
                providers: Vec::new(),
            },
            selected: 0,
            notify,
            flash: None,
        }
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.snapshot.sessions.get(self.selected)
    }

    /// Replaces the data, keeping the same session selected when it still exists
    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        let keep = self.selected_session().map(Session::key);
        self.snapshot = snapshot;
        let found = keep.and_then(|k| self.snapshot.sessions.iter().position(|s| s.key() == k));
        let last = self.snapshot.sessions.len().saturating_sub(1);
        self.selected = found.unwrap_or(self.selected).min(last);
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // raw mode delivers Crtl-C as a key, not as a signal
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.snapshot.sessions.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('n') => self.notify = !self.notify,
            _ => {}
        }
        Action::None
    }
}

// TODO, all tests
// #[cfg(test)]
// mod tests {
//     use super::*;
// }
