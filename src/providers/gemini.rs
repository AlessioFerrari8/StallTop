//! Gemini CLI: liveness from running processes, activity from the chat files in `tmp/<project id>/chats/`. Two chat formats exist: 
//! append-only `session-*.jsonl` (Gemini CLI 0.60+) and whole-file `session-*.json` (older versions). See docs/DATA-SOURCES.md. 
//! Gemini CLI does not expose a permission state, so sessions are only working / idle / ended

use std::{collections::{HashMap, HashSet}, path::{Path, PathBuf}, time::SystemTime};

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{model::{Activity, Agent, DayTotals, Event, EventKind, ProviderState, Session, TokenUsage, into_session}, pricing::cost_usd, procfs::{ProcInfo, ProcessTable}, providers::{ProviderReport, RESCAN_EVERY, clip, local_day, modified_on, mtime}, status::{gemini_status, keep_dead}, tail::JsonlTail};


const WRITE_TOOLS: [&str; 3] = ["write_file", "replace", "edit"];

/// What stalltop knows about one chat, whaterver its format
#[derive(Debug, Clone, Default)]
pub struct Chat {
    pub session_id: Option<String>,
    pub start_time: Option<DateTime<Utc>>,
    pub activity: Activity,
}

impl Chat {
    fn from_messages(session_id: Option<String>, start_time: Option<DateTime<Utc>>, messages: &[Value]) -> Self {
        let mut activity = Activity::default();
        for m in messages {
            add_message(&mut activity, m);
        }
        Self {
            session_id,
            start_time,
            activity,
        }
    }
}

/// Parses a whole legacy `.json` chat. None when it is not valid JSON (for example caught mid-write)
pub fn parse_chat(json: &str) -> Option<Chat> {
    let v: Value = serde_json::from_str(json).ok()?;
    Some(Chat::from_messages(
        v["sessionId"].as_str().map(String::from), 
        parse_time(&v["startTime"]), 
        v["messages"].as_array().map(Vec::as_slice).unwrap_or(&[])))
}

/// Replays a `.jsonl` chat line by line, the way Gemini CLI's own loader (`loadConversationRecord`) does. Each line = one record
#[derive(Debug, Default)]
pub struct ChatLog {
    session_id: Option<String>,
    start_time: Option<DateTime<Utc>>,
    /// In first-seen order
    messages: Vec<Value>,
    /// Message id -> index in `messages`
    index: HashMap<String, usize>,
    pub skipped_lines: u64,
}

impl ChatLog {
    pub fn ingest(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            self.skipped_lines += 1;
            return;
        };
        if let Some(id) = v["$rewindTo"].as_str() {
            self.rewind(id);
        } else if v["id"].is_string() {
            self.upsert(v);
        } else if v["$set"].is_object() {
            let set = &v["$set"];
            self.metadata(set);
            if let Some(messages) = set["messages"].as_array() {
                self.messages.clear();
                self.index.clear();
                for m in messages.iter().filter(|m| m["id"].is_string()) {
                    self.upsert(m.clone());
                }
            }
        } else if v["sessionId"].is_string() && v["projectHash"].is_string() {
            self.metadata(&v);
        } else {
            self.skipped_lines += 1;
        }
    }

    pub fn chat(&self) -> Chat {
        Chat::from_messages(self.session_id.clone(), self.start_time, &self.messages)
    }

    fn metadata(&mut self, v: &Value) {
        if let Some(id) = v["sessionId"].as_str() {
            self.session_id = Some(id.to_string());
        }
        if let Some(t) = parse_time(&v["startTime"]) {
            self.start_time = Some(t);
        }
    }

    fn upsert(&mut self, message: Value) {
        let id = message["id"].as_str().unwrap_or_default().to_string();
        let message = compact(&message);
        match self.index.get(&id) {
            Some(&i) => self.messages[i] = message,
            None => {
                self.index.insert(id, self.messages.len());
                self.messages.push(message);
            }
        }
    }

    /// Like Gemini CLI: an unknown id clears everything
    fn rewind(&mut self, id: &str) {
        let keep = self.index.get(id).copied().unwrap_or(0);
        self.messages.truncate(keep);
        self.index.retain(|_, i| *i < keep);
    }
}


/// Keeps only what `add_message` reads. A message record carries the full text of replies and tool arguments (whole files for
/// `write_file`): a long chat would otherwise stay in memory in full
fn compact(m: &Value) -> Value {
    let calls: Vec<Value> = m["tooCalls"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|c| {
            let a = &c["args"];
            json!({
                "name": c["name"],
                "args": {
                    "file_path": a["file_path"],
                    "absolute_path": a["absolute_path"],
                    "command": &a["command"].as_str().map(|s| clip(s, 60)),
                },
            })
        })
        .collect();
    json!({
        "id": m["id"],
        "timestamp": m["timestamp"],
        "type": m["type"],
        "model": m["model"],
        "tokens": m["tokens"],
        "content": clip(&content_text(&m["content"]), 120),
        "toolCalls": calls,
    })
}

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| s.parse().ok())
}

/// Adds one message (either format) to the activity
fn add_message(a: &mut Activity, m: &Value) {
    let Some(ts) = parse_time(&m["timestamp"]) else {
        return;
    };
    let text = content_text(&m["content"]);
    match m["type"].as_str() {
        Some("user") if !text.trim().is_empty() => a.push_event(Event {
            at: ts,
            kind: EventKind::Prompt,
            text: clip(&text, 120),
        }),
        Some("gemini") => {
            let model = m["model"].as_str();
            if let Some(model) = model {
                a.model = Some(model.to_string());
            }
            let t = &m["tokens"];
            if t.is_object() {
                let n = |k: &str| t[k].as_u64().unwrap_or(0);
                let usage = TokenUsage {
                    input: n("input").saturating_sub(n("cached")),
                    output: n("output") + n("thoughts"),
                    cache_read: n("cached"),
                    ..Default::default()
                };
                a.add_usage(ts, &usage, model.and_then(|m| cost_usd(m, &usage)));
            }
            for call in m["toolCalls"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                add_tool_call(a, call, ts);
            }
            if !text.trim().is_empty() {
                a.push_event(Event {
                    at: ts,
                    kind: EventKind::Reply,
                    text: clip(&text, 120),
                });
            }
        }
        Some("error") => a.push_event(Event {
            at: ts,
            kind: EventKind::Error,
            text: clip(&text, 120),
        }),
        _ => a.touch(ts),
    }
}

fn add_tool_call(a: &mut Activity, call: &Value, ts: DateTime<Utc>) {
    let name = call["name"].as_str().unwrap_or("tool");
    let file = call["args"]["file_path"]
        .as_str()
        .or(call["args"]["absolute_path"].as_str());
    let desc = match (file, call["args"]["command"].as_str()) {
        (Some(f), _) => format!(
            "{name} {}",
            Path::new(f)
                .file_name()
                .map(|b| b.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        (None, Some(c)) => clip(&format!("{name} {c}"), 60),
        _ => name.to_string(),
    };
    if let (true, Some(f)) = (WRITE_TOOLS.contains(&name), file) {
        a.files_touched.insert(PathBuf::from(f));
    }
    *a.tool_counts.entry(name.to_string()).or_default() += 1;
    a.last_tool = Some(desc.clone());
    a.push_event(Event {
        at: ts,
        kind: EventKind::Tool,
        text: desc,
    });
}

/// Content is either a string or a list of parts with `text`
fn content_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Before 0.60 Gemini CLI named the per-project directory after the SHA-256 of the project root
/// 0.60 copies such directories to the short id on first start
pub fn project_hash(cwd: &Path) -> String {
    Sha256::digest(cwd.to_string_lossy().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `~/.gemini/projects.json` (Gemini CLI 0.60+): `{"projects":{"<project root>":"<short id>"}}`
pub fn parse_projects(json: &str) -> HashMap<PathBuf, String> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return HashMap::new();
    };
    v["projects"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(path, id)| Some((PathBuf::from(path), id.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Directories under `tmp` that may hold the chats of a project: its short id (if registered), then the legacy
/// hash directory
pub fn project_dirs(tmp: &Path, cwd: &Path, ids: &HashMap<PathBuf, String>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ids.get(cwd).map(|id| tmp.join(id)).into_iter().collect();
    dirs.push(tmp.join(project_hash(cwd)));
    dirs
}

/// Chat files (`.json` or `.jsonl`) directly in a `chats` directory. Subagent chats live in subdirectories and backups have
/// other extensions: both are left out
fn chat_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries 
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json" || e == "jsonl"))
        .collect()
}

/// Like Gemini CLI's `sanitizeFilenamePart`
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c 
            } else {
                '_'
            }
        })
        .collect()
}

/// Subagent chats of a session: `chats/<parent sessionId>/<subagent sessionId>.jsonl`, next to the parent's
/// chat file
pub fn subagent_chats(chat: &Path, session_id: &str) -> Vec<PathBuf> {
    let Some(dir) = chat.parent() else { return Vec::new() };
    chat_files(&dir.join(sanitize(session_id)))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect() 
}


/// Every subanget chat under a `chats` directory, whatever its parent
fn all_subagent_chats(chats: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(chats) else {
        return Vec::new();
    };
    entries.
        flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .flat_map(|d| chat_files(&d))
        .collect()
}

/// A js runtimne: `node`, `bun`, `deno`, or a versioned `node22`
pub(crate) fn is_runtime(name: &str) -> bool {
    matches!(name, "node" | "bun" | "deno")
        || name
            .strip_prefix("node")
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
}

/// `gemini` itself, `node .../bin/gemini`, or a script from the @google/gemini-cli package
pub fn is_gemini(argv: &[String]) -> bool {
    let base = |a: &String| {
        Path::new(a)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let Some(first) = argv.first() else { return false };
    if base(first) == "gemini" {
        return true;
    }
    is_runtime(&base(first))
        && argv
            .get(1)
            .is_some_and(|a| base(a) == "gemini" || a.contains("@google/gemini-cli"))
}

enum Source {
    // Legacy `.json`: re-parsed whole when its mtime changes
    Whole,
    // `.jsonl`: only new lines are read
    Log(JsonlTail, ChatLog),
}

struct Cached {
    modified: SystemTime,
    source: Source,
    chat: Chat,
}

struct Seen {
    cwd: PathBuf,
    // Process start time (clock ticks), to decide which process owns a shared chat
    proc_start: u64,
    chat_path: Option<PathBuf>,
    // Chats of the subagents the session started
    subagents: Vec<PathBuf>,
    started_at: DateTime<Utc>,
    dead_since: Option<DateTime<Utc>>,
}

pub struct GeminiProvider {
    root: PathBuf,
    polls: u64,
    chats: HashMap<PathBuf, Cached>,
    // by pid
    seen: HashMap<u32, Seen>,
}

impl GeminiProvider {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            polls: 0,
            chats: HashMap::new(),
            seen: HashMap::new(),
        }
    }

    fn load(&mut self, path: &Path) {
        let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            return;
        };
        if path.extension().is_some_and(|e| e == "jsonl") {
            self.load_log(path, modified);
        } else {
            self.load_whole(path, modified);
        }
    }

    /// Re-parses a legacy chat only when its mtime changed
    fn load_whole(&mut self, path: &Path, modified: SystemTime) {
        if self.chats.get(path).is_some_and(|c| c.modified == modified) {
            return;
        }
        if let Some(chat) = std::fs::read_to_string(path).ok().and_then(|s| parse_chat(&s)) {
            let source = Source::Whole;
            self.chats.insert(path.to_path_buf(), Cached { modified, source, chat });
        }
    }

    // reads the line appended since the last call
    fn load_log(&mut self, path: &Path, modified: SystemTime) {
        let c = self.chats.entry(path.to_path_buf()).or_insert_with(|| Cached {
            modified,
            source: Source::Log(JsonlTail::new(path), ChatLog::default()),
            chat: Chat::default(),
        });
        let Source::Log(tail, log) = &mut c.source else { return };
        let Ok(read) = tail.read_new() else { return };
        if read.reset {
            *log = ChatLog::default();
        } 
        for line in &read.lines {
            log.ingest(line);
        }
        c.modified = modified;
        if read.reset || !read.lines.is_empty() {
            c.chat = log.chat();
        }
    }

    fn read_projects(&self) -> HashMap<PathBuf, String> {
        std::fs::read_to_string(self.root.join("projects.json"))
            .map(|s| parse_projects(&s))
            .unwrap_or_default()
    }

    fn newest_chat(&self, cwd: &Path, ids: &HashMap<PathBuf, String>) -> Option<PathBuf> {
        project_dirs(&self.root.join("tmp"), cwd, ids)
            .iter()
            .flat_map(|d| chat_files(&d.join("chats")))
            .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
    }

    /// Loads the chats modified today and forgets those neither in use nor from today
    fn scan_today(&mut self, now: DateTime<Utc>) {
        let day = local_day(now);
        let in_use: HashSet<&PathBuf> = self
            .seen
            .values()
            .flat_map(|s| s.chat_path.iter().chain(&s.subagents))
            .collect();
        self.chats.retain(|p, _| in_use.contains(p) || modified_on(p, day));
        let Ok(dirs) = std::fs::read_dir(self.root.join("tmp")) else {
            return;
        };
        let files: Vec<PathBuf> = dirs
            .flatten()
            .flat_map(|d| {
                let chats = d.path().join("chats");
                let mut files = chat_files(&chats);
                files.extend(all_subagent_chats(&chats));
                files
            })
            .filter(|p| modified_on(p, day))
            .collect();

        for p in files {
            self.load(&p);
        }

    }

    fn track(&mut self, running: &[&ProcInfo], now: DateTime<Utc>) {
        let ids = self.read_projects();
        for p in running {
            let cwd = p.cwd.clone().expect("filtered by the caller");
            let chat_path = self.newest_chat(&cwd, &ids);
            if let Some(c) = &chat_path {
                self.load(c);
            }
            let chat = chat_path.as_ref().and_then(|c| self.chats.get(c)).map(|c| &c.chat);
            let started_at = chat.and_then(|c| c.start_time).unwrap_or(now);
            let subagents = match (&chat_path, chat.and_then(|c| c.session_id.as_deref())) {
                (Some(path), Some(id)) => subagent_chats(path, id),
                _ => Vec::new(),
            };
            for s in &subagents {
                self.load(s);
            }
            let e = self.seen.entry(p.pid).or_insert(Seen {
                cwd: cwd.clone(),
                proc_start: p.start_time,
                chat_path: None,
                subagents: Vec::new(),
                started_at,
                dead_since: None,
            });
            e.cwd = cwd;
            e.chat_path = chat_path;
            e.subagents = subagents;
            e.dead_since = None;
        }

        for (pid, s) in self.seen.iter_mut() {
            if s.dead_since.is_none() && !running.iter().any(|p| p.pid == *pid) {
                s.dead_since = Some(now);
            }
        }
        self.seen.retain(|_, s| s.dead_since.is_none_or(|d| keep_dead(d, now)));
    }

    /// One row per process. Seeral processes in the same directory find the same newest chat: only the first
    /// (live before dead, then oldest) shows it, the others are listed as `pid-N` without activity, so keys stau unique
    fn sessions(&self, now: DateTime<Utc>) -> Vec<Session> {
        let mut order: Vec<(&u32, &Seen)> = self.seen.iter().collect();
        order.sort_by_key(|(pid, s)| (s.dead_since.is_some(), s.proc_start, **pid));
        let empty = Chat::default();
        let mut claimed = HashSet::new();
        let mut sessions: Vec<Session> = order
            .into_iter()
            .map(|(pid, s)| {
                let chat = s
                    .chat_path
                    .as_ref()
                    .and_then(|c| self.chats.get(c))
                    .map(|c| &c.chat)
                    .filter(|c| c.session_id.as_ref().is_none_or(|id| claimed.insert(id.clone())));
                let owns_chat = chat.is_some();
                let chat = chat.unwrap_or(&empty);
                // Subagent usage is not repeated in the parent chat: fold it in, and count a subagent's writes
                // as the parent working
                let mut activity = chat.activity.clone();
                let mut last_write = s.chat_path.as_deref().and_then(mtime);
                if owns_chat {
                    for sub in &s.subagents {
                        if let Some(c) = self.chats.get(sub) {
                            activity.merge_usage(&c.chat.activity);
                        }
                        last_write = last_write.max(mtime(sub));
                    }
                }
                let status = gemini_status(s.dead_since, last_write, now);
                let id = chat.session_id.clone().unwrap_or_else(|| format!("pid-{pid}"));
                into_session(
                    Agent::Gemini, 
                    &id, 
                    Some(*pid), 
                    &s.cwd, 
                    s.started_at, 
                    status, 
                    &activity, 
                    now,
                )
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }

}


/// Sums one day over every known chat, counting each session once: a legacy chat resumed by Gemini CLI 0.60 is copied to
/// `<name>.jsonl`, and its storage migration copies whole project directories, so one session can be in several files.
/// The most recently modified copy wins
fn day_totals<'a>(chats: impl Iterator<Item = (&'a PathBuf, &'a Cached)>, day: NaiveDate) -> DayTotals {
    let mut newest: HashMap<String, &Cached> = HashMap::new();
    for (path, c) in chats {
        let key = c.chat.session_id.clone().unwrap_or_else(|| path.display().to_string());
        let slot = newest.entry(key).or_insert(c);
        if c.modified > slot.modified {
            *slot = c;
        }
    }
    let mut today = DayTotals::default();
    for c in newest.values() {
        today.add(&c.chat.activity.day(day));
    }
    today
}

impl super::Provider for GeminiProvider {
    fn poll(&mut self, now: DateTime<Utc>, procs: &dyn ProcessTable) -> ProviderReport {
        let found = self.root.is_dir();
        if found && self.polls.is_multiple_of(RESCAN_EVERY) {
            self.scan_today(now);
        }
        self.polls += 1;

        // Without ~/.gemini there is nothing to show: do not even scan /proc
        let all = if found { procs.all() } else { Vec::new() };
        let gemini_pids: Vec<u32> = all.iter().filter(|p| is_gemini(&p.argv)).map(|p| p.pid).collect();
        // Skip helper processes whose parent is itself a Gemini CLI process
        let running: Vec<&ProcInfo> = all   
            .iter()
            .filter(|p| gemini_pids.contains(&p.pid) && !gemini_pids.contains(&p.ppid) && p.cwd.is_some())
            .collect();
        self.track(&running, now);

        let skipped = self
            .chats
            .values()
            .map(|c| match &c.source {
                Source::Log(_, log) => log.skipped_lines,
                Source::Whole => 0,
            })
            .sum();
        ProviderReport { 
            sessions: self.sessions(now), 
            today: day_totals(self.chats.iter(), local_day(now)), 
            state: ProviderState {
                name: "gemini",
                found,
                skipped_lines: skipped,
            },
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use crate::procfs::FakeProcs;
//     use crate::providers::Provider;
    
// }