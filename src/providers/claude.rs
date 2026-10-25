//! Claude Code: live state from `session/<pid>.json`, activity from `projects/<econded-cwd>/<sessionId>.jsonl`.
//! See docs/DATA-SOURCES.md

use super::{ProviderReport, RESCAN_EVERY, clip, local_day, modified_on};
use crate::limits::{LimitHit, parse_limit};
use crate::model::{Activity, Agent, DayTotals, Event, EventKind, ProviderState, Session, TokenUsage, into_session};
use crate::pricing::cost_usd;
use crate::procfs::ProcessTable;
use crate::status::{ClaudeFacts, LiveState, claude_status, keep_dead};
use crate::tail::JsonlTail;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Tools whose `file_path` / `notebook_path` input is a file the agent changed
const WRITE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Everything derived from 1 transcript
#[derive(Default)]
pub struct Transcript {
    pub activity: Activity,
    /// Set while the last meaningful line is a usage-limit error
    pub pending_limit: Option<LimitHit>,
    pub skipped_lines: u64,
    /// (message.id, requestId) pairs already billed: one API message is often written as several
    /// lines that repeat the same usage
    billed: HashSet<(String, String)>,
}

impl Transcript {
    pub fn ingest(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            self.skipped_lines += 1;
            return;
        };
        let ts = v["timestamp"].as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok());
        match v["type"].as_str() {
            Some("ai-title") => {
                if let Some(t) = v["aiTitle"].as_str() {
                    self.activity.title = Some(t.to_string());
                }
            }
            Some("assistant") => {
                if let Some(ts) = ts {
                    self.assistant(&v, ts);
                }
            }
            Some("user") => {
                if let Some(ts) = ts {
                    self.user(&v, ts);
                }
            }
            Some("system") => {
                // "Usage limit reset · continuing automatically"
                if v["content"]
                    .as_str()
                    .is_some_and(|c| c.to_lowercase().contains("limit reset"))
                {
                    self.pending_limit = None;
                }
                if let Some(ts) = ts {
                    self.activity.touch(ts);
                }
            }
            Some(_) => {
                if let Some(ts) = ts {
                    self.activity.touch(ts);
                }
            }
            None => self.skipped_lines += 1,
        }
    }

    fn assistant(&mut self, v: &Value, ts: DateTime<Utc>) {
        let msg = &v["message"];
        let blocks = msg["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if v["isApiErrorMessage"].as_bool() == Some(true) || v["error"].as_str() == Some("rate_limit") {
            let text = blocks.iter().find_map(|b| b["text"].as_str()).unwrap_or("API error");
            if let Some(hit) = parse_limit(text, ts) {
                self.pending_limit = Some(hit);
            }
            self.activity.push_event(Event {
                at: ts,
                kind: EventKind::Error,
                text: clip(text, 120),
            });
            return;
        }
        self.pending_limit = None;
        let model = msg["model"].as_str().filter(|m| !m.starts_with('<'));
        if let Some(m) = model {
            self.activity.model = Some(m.to_string());
        }

        let key = (
            msg["id"].as_str().unwrap_or_default().to_string(),
            v["requestId"].as_str().unwrap_or_default().to_string(),
        );
        // without a message id there is nothing to deduplicate on: bill the line
        let first_time = key.0.is_empty() || self.billed.insert(key);
        if first_time && msg["usage"].is_object() {
            let usage = parse_usage(&msg["usage"]);
            let cost = model.and_then(|m| cost_usd(m, &usage));
            self.activity.add_usage(ts, &usage, cost);
        }

        for b in blocks {
            match b["type"].as_str() {
                Some("tool_use") => {
                    let name = b["name"].as_str().unwrap_or("tool");
                    let input = &b["input"];
                    let desc = describe_tool(name, input);
                    *self.activity.tool_counts.entry(name.to_string()).or_default() += 1;
                    if WRITE_TOOLS.contains(&name)
                        && let Some(p) = input["file_path"].as_str().or(input["notebook_path"].as_str())
                    {
                        self.activity.files_touched.insert(PathBuf::from(p));
                    }
                    self.activity.last_tool = Some(desc.clone());
                    self.activity.push_event(Event {
                        at: ts,
                        kind: EventKind::Tool,
                        text: desc,
                    });
                }
                Some("text") => {
                    let text = b["text"].as_str().unwrap_or_default();
                    if !text.trim().is_empty() {
                        self.activity.push_event(Event {
                            at: ts,
                            kind: EventKind::Reply,
                            text: clip(text, 120),
                        });
                    }
                }
                _ => self.activity.touch(ts),
            }
        }
    }

    fn user(&mut self, v: &Value, ts: DateTime<Utc>) {
        if v["isMeta"].as_bool() == Some(true) {
            self.activity.touch(ts);
            return;
        }
        match v["message"]["content"].as_str() {
            // slash commands and injected context start with a tag; they are not prompts
            Some(text) if !text.trim_start().starts_with('<') => {
                self.activity.push_event(Event {
                    at: ts,
                    kind: EventKind::Prompt,
                    text: clip(text, 120),
                });
            }
            _ => self.activity.touch(ts),
        }
    }
}

fn parse_usage(u: &Value) -> TokenUsage {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let creation = n(&u["cache_creation_input_tokens"]);
    let split = &u["cache_creation"];
    let (w5, w1) = if split.is_object() {
        (
            n(&split["ephemeral_5m_input_tokens"]),
            n(&split["ephemeral_1h_input_tokens"]),
        )
    } else {
        (creation, 0)
    };
    TokenUsage {
        input: n(&u["input_tokens"]),
        output: n(&u["output_tokens"]),
        cache_write_5m: w5,
        cache_write_1h: w1,
        cache_read: n(&u["cache_read_input_tokens"]),
    }
}

/// "Edit Foo.java", "Bash npm run build", "Grep TODO", or just the tool name
pub fn describe_tool(name: &str, input: &Value) -> String {
    let file = input["file_path"]
        .as_str()
        .or(input["notebook_path"].as_str())
        .or(input["path"].as_str());
    if let Some(f) = file {
        let base = Path::new(f)
            .file_name()
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or_else(|| f.to_string());
        return format!("{name} {base}");
    }
    for key in ["command", "pattern", "url", "query", "description"] {
        if let Some(s) = input[key].as_str() {
            return clip(&format!("{name} {s}"), 60);
        }
    }
    name.to_string()
}

/// One `sessions/<pid>.json` file
#[derive(Debug, Clone, PartialEq)]
pub struct SessionFile {
    pub pid: u32,
    pub session_id: String,
    pub cwd: PathBuf,
    pub started_at: DateTime<Utc>,
    pub proc_start: Option<u64>,
    pub live: LiveState,
}

pub fn parse_session_file(json: &str) -> Option<SessionFile> {
    let v: Value = serde_json::from_str(json).ok()?;
    let ms = |v: &Value| v.as_i64().and_then(|ms| Utc.timestamp_millis_opt(ms).single());
    let started_at = ms(&v["startedAt"])?;
    let live = match v["status"].as_str() {
        Some("waiting") => LiveState::Waiting {
            what: v["waitingFor"].as_str().unwrap_or("input").to_string(),
            since: ms(&v["statusUpdatedAt"]).unwrap_or(started_at),
        },
        Some("busy") => LiveState::Busy,
        _ => LiveState::Idle,
    };
    let proc_start = match &v["procStart"] {
        Value::String(s) => s.parse().ok(),
        other => other.as_u64(),
    };
    Some(SessionFile {
        pid: u32::try_from(v["pid"].as_u64()?).ok()?,
        session_id: v["sessionId"].as_str()?.to_string(),
        cwd: PathBuf::from(v["cwd"].as_str()?),
        started_at,
        proc_start,
        live,
    })
}

/// One transcript file, read incrementally
struct Tailed {
    tail: JsonlTail,
    transcript: Transcript,
}

impl Tailed {
    fn new(path: &Path) -> Self {
        Self {
            tail: JsonlTail::new(path),
            transcript: Transcript::default(),
        }
    }

    /// Ingests what was appended since the last call
    fn read(&mut self) {
        let Ok(read) = self.tail.read_new() else { return };
        if read.reset {
            self.transcript = Transcript::default();
        }
        for line in &read.lines {
            self.transcript.ingest(line);
        }
    }
}

/// A session's main transcript plus the transcripts of the subagents it ran
#[derive(Default)]
struct Tracked {
    /// None until the transcript file exists
    main: Option<Tailed>,
    /// `<sessionId>/subagents/*.jsonl` by path
    subagents: HashMap<PathBuf, Tailed>,
}

impl Tracked {
    fn all(&self) -> impl Iterator<Item = &Tailed> {
        self.main.iter().chain(self.subagents.values())
    }

    /// The main transcript's activity with the subagents' usage folded in, so a session running a
    /// foreground subagent is neither cheaper nor "quiet"
    fn activity(&self) -> Activity {
        let mut a = self
            .main
            .as_ref()
            .map(|m| m.transcript.activity.clone())
            .unwrap_or_default();
        for s in self.subagents.values() {
            a.merge_usage(&s.transcript.activity);
        }
        a
    }

    fn pending_limit(&self) -> Option<&LimitHit> {
        self.main.as_ref().and_then(|m| m.transcript.pending_limit.as_ref())
    }

    fn modified_on(&self, day: NaiveDate) -> bool {
        self.all().any(|t| modified_on(t.tail.path(), day))
    }
}

/// Claude Code writes the transcripts of subagents (Task tool) next to the main transcript: `<dir>/<sessionId>.jsonl` ->
/// `<dir>/<sessionId>/subagents/`
pub fn subagent_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("").join("subagents")
}

fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect()
}

struct Live {
    file: SessionFile,
    dead_since: Option<DateTime<Utc>>,
}

pub struct ClaudeProvider {
    root: PathBuf,
    polls: u64,
    // by session id, every transcript touched today + every listed session's
    transcripts: HashMap<String, Tracked>,
    // by pid
    live: HashMap<u32, Live>,
    // (pid, session id) of dead sessions already shown for `KEEP_DEAD_SECS` whose session file is still
    // on disk (for example after a crash): not listed again
    expired: HashSet<(u32, String)>,
}

impl ClaudeProvider {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            polls: 0,
            transcripts: HashMap::new(),
            live: HashMap::new(),
            expired: HashSet::new(),
        }
    }

    fn find_trancsript(&self, session_id: &str) -> Option<PathBuf> {
        let file = format!("{session_id}.jsonl");
        std::fs::read_dir(self.root.join("projects"))
            .ok()?
            .flatten()
            .map(|d| d.path().join(&file))
            .find(|p| p.is_file())
    }

    /// Reads new lines of a session's transcript and of its subagents' transcripts
    fn refresh(&mut self, session_id: &str) {
        let missing = self.transcripts.get(session_id).is_none_or(|t| t.main.is_none());
        let found = missing.then(|| self.find_trancsript(session_id)).flatten();
        let t = self.transcripts.entry(session_id.to_string()).or_default();

        if let Some(p) = found {
            t.main = Some(Tailed::new(&p));
        }

        let Some(main) = t.main.as_mut() else { return };
        main.read();
        for p in jsonl_files(&subagent_dir(main.tail.path())) {
            t.subagents.entry(p.clone()).or_insert_with(|| Tailed::new(&p));
        }
        for s in t.subagents.values_mut() {
            s.read();
        }
    }

    /// Tracks transcripts modified today so "today" totals include ended sessions, and forget the ones that
    /// are neither listed nor from today
    fn scan_today(&mut self, now: DateTime<Utc>) {
        let day = local_day(now);
        let listed: HashSet<&String> = self.live.values().map(|l| &l.file.session_id).collect();
        self.transcripts
            .retain(|id, t| listed.contains(id) || t.modified_on(day));
        let Ok(dirs) = std::fs::read_dir(self.root.join("projects")) else {
            return;
        };
        let files: Vec<PathBuf> = dirs
            .flatten()
            .flat_map(|d| jsonl_files(&d.path()))
            .filter(|p| modified_on(p, day))
            .collect();
        for p in files {
            let Some(id) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let t = self.transcripts.entry(id.clone()).or_default();
            if t.main.is_none() {
                t.main = Some(Tailed::new(&p));
            }
            self.refresh(&id);
        }
    }

    fn read_session_files(&self) -> Vec<SessionFile> {
        let Ok(entries) = std::fs::read_dir(self.root.join("sessions")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .filter_map(|s| parse_session_file(&s))
            .collect()
    }
}

impl super::Provider for ClaudeProvider {
    fn poll(&mut self, now: DateTime<Utc>, procs: &dyn ProcessTable) -> ProviderReport {
        let found = self.root.is_dir();
        if found && self.polls.is_multiple_of(RESCAN_EVERY) {
            self.scan_today(now);
        }
        self.polls += 1;

        let files = self.read_session_files();
        let current: HashSet<u32> = files.iter().map(|f| f.pid).collect();
        self.expired
            .retain(|(pid, id)| files.iter().any(|f| f.pid == *pid && &f.session_id == id));
        for f in files {
            if self.expired.contains(&(f.pid, f.session_id.clone())) {
                continue;
            }
            let alive = procs
                .get(f.pid)
                .is_some_and(|p| f.proc_start.is_none_or(|s| s == p.start_time));
            let entry = self.live.entry(f.pid).or_insert(Live {
                file: f.clone(),
                dead_since: None,
            });
            entry.file = f;
            if alive {
                entry.dead_since = None;
            } else if entry.dead_since.is_none() {
                entry.dead_since = Some(now);
            }
        }

        // Claude code removes its session file on exit
        for (pid, l) in self.live.iter_mut() {
            if !current.contains(pid) && l.dead_since.is_none() {
                l.dead_since = Some(now);
            }
        }
        let expired = &mut self.expired;
        self.live.retain(|pid, l| {
            let keep = l.dead_since.is_none_or(|d| keep_dead(d, now));
            if !keep {
                expired.insert((*pid, l.file.session_id.clone()));
            }
            keep
        });

        let ids: Vec<String> = self
            .live
            .values()
            .filter(|l| l.dead_since.is_none())
            .map(|l| l.file.session_id.clone())
            .collect();
        for id in ids {
            self.refresh(&id);
        }

        let empty = Tracked::default();
        let mut sessions: Vec<Session> = self
            .live
            .values()
            .map(|l| {
                let t = self.transcripts.get(&l.file.session_id).unwrap_or(&empty);
                let activity = t.activity();
                let facts = ClaudeFacts {
                    dead_since: l.dead_since,
                    live: &l.file.live,
                    pending_limit: t.pending_limit(),
                    last_write: activity.last_activity,
                };
                let status = claude_status(&facts, now);
                into_session(
                    Agent::Claude,
                    &l.file.session_id,
                    Some(l.file.pid),
                    &l.file.cwd,
                    l.file.started_at,
                    status,
                    &activity,
                    now,
                )
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));

        let day = local_day(now);
        let mut today = DayTotals::default();
        let mut skipped = 0;
        for t in self.transcripts.values().flat_map(Tracked::all) {
            today.add(&t.transcript.activity.day(day));
            skipped += t.transcript.skipped_lines;
        }
        ProviderReport {
            sessions,
            today,
            state: ProviderState {
                name: "claude",
                found,
                skipped_lines: skipped,
            },
        }
    }
}

// test
