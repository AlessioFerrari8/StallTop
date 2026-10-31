//! Antigravity CLI (`agy`): liveness from running processes, activity from `brain/<conversation>/.system_generated/logs/transcript.jsonl`.
//! The transcript does not say where the session runs: that comes from `log/cli-*.log`, which names the workspace of each `agy`
//! process and the conversations it created. Like Gemini CLI it exposes no permission state, so sessions are only working / idle / ended
//! See docs/DATA-SOURCES.md

use std::{collections::{HashMap, HashSet}, path::{Path, PathBuf}};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{model::{Activity, Agent, DayTotals, Event, EventKind, ProviderState, Session, TokenUsage, into_session}, pricing::cost_usd, procfs::{ProcInfo, ProcessTable}, providers::{ProviderReport, RESCAN_EVERY, clip, local_day, modified_on, mtime}, status::{gemini_status, keep_dead}, tail::JsonlTail};

const WRITE_TOOLS: [&str; 4] = [
    "write_to_file",
    "replace_file_content",
    "multi_replace_file_content",
    "edit_file",
];
const FILE_ARGS: [&str; 3] = ["TargetFile", "AbsolutePath", "FilePath"];

/// Everything read from one transcript so far
#[derive(Default)]
pub struct Transcript {
    pub activity: Activity,
    pub skipped_lines: u64,
}

impl Transcript {
    pub fn ingest(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            self.skipped_lines += 1;
            return;
        };
        let Some(ts) = v["created_at"].as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok()) else {
            self.skipped_lines += 1;
            return;
        };
        let a = &mut self.activity;
        match v["type"].as_str() {
            Some("USER_INPUT") => {
                let text = v["content"].as_str().unwrap_or("");
                if let Some(model) = model_change(text) {
                    a.model = Some(model);
                }
                let request = between(text, "<USER_REQUEST>", "</USER_REQUEST>").unwrap_or(text);
                if !request.trim().is_empty() {
                    a.push_event(Event {
                        at: ts,
                        kind: EventKind::Prompt,
                        text: clip(request, 120),
                    });
                }
            }
            Some("PLANNER_RESPONSE") => {
                let n = |k: &str| v[k].as_u64().unwrap_or(0);
                let usage = TokenUsage {
                    input: n("input_tokens"),
                    output: n("output_tokens"),
                    cache_read: n("cache_read_tokens"),
                    ..Default::default()
                };
                let cost = a.model.as_deref().and_then(|m| cost_usd(m, &usage));
                a.add_usage(ts, &usage, cost);
                for call in v["tool_calls"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                    add_tool_call(a, call, ts);
                }
                if let Some(text) = v["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    a.push_event(Event {
                        at: ts,
                        kind: EventKind::Reply,
                        text: clip(text, 120),
                    });
                }
            }
            _ if v["status"] == "ERROR" => {
                let text = v["error"].as_str().or(v["content"].as_str()).unwrap_or("error");
                a.push_event(Event {
                    at: ts,
                    kind: EventKind::Error,
                    text: clip(text, 120),
                });
            }
            _ => a.touch(ts),
        }
    }
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let len = s[start..].find(close)?;
    Some(&s[start..start + len])
}

/// "The user changed setting `Model Selection` from None to Gemini 3.8 Flash (High). No need..."
fn model_change(text: &str) -> Option<String> {
    let rest = text.split_once("`Model Selection` from ")?.1;
    let (_, to) = rest.split_once(" to ")?;
    let model = to.split(". No need").next()?.trim();
    (!model.is_empty()).then(|| model.to_string())
}

/// Tool arguments are JSON values encoded again as strings: `"\"/p/a.rs\""`.
fn arg(call: &Value, key: &str) -> Option<String> {
    let raw = call["args"][key].as_str()?;
    Some(serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string()))
}

fn add_tool_call(a: &mut Activity, call: &Value, ts: DateTime<Utc>) {
    let name = call["name"].as_str().unwrap_or("tool");
    let file = FILE_ARGS.iter().find_map(|k| arg(call, k));
    let desc = match (&file, arg(call, "CommandLine")) {
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


/// Workspace and conversations of every `agy` process, from one `cli-*.log`: conversation id -> workspace directories
pub fn parse_log(text: &str) -> HashMap<String, Vec<PathBuf>> {
    let mut workspaces: Vec<PathBuf> = Vec::new();
    let mut ids = Vec::new();
    for line in text.lines() {
        if workspaces.is_empty() && let Some(list) = between(line, "workspaceDirs=[", "]") {
            workspaces = list.split_whitespace().map(PathBuf::from).collect();
        }
        if let Some(rest) = line.split_once("Created conversation ").map(|x| x.1) && let Some(id) = rest.split_whitespace().next() {
            ids.push(id.to_string());
        }
    }
    ids.into_iter().map(|id| (id, workspaces.clone())).collect()
}


/// `agy` itself or a helper started by it
pub fn is_agy(argv: &[String]) -> bool {
    argv.first()
        .and_then(|a| Path::new(a).file_name())
        .is_some_and(|n| n == "agy")
}

struct Cached {
    tail: JsonlTail,
    transcript: Transcript,
}

struct Seen {
    cwd: PathBuf,
    proc_start: u64,
    conversation: Option<String>,
    started_at: DateTime<Utc>,
    dead_since: Option<DateTime<Utc>>,
}

pub struct AntigravityProvider {
    root: PathBuf,
    polls: u64,
    // by conversation id
    transcripts: HashMap<String, Cached>,
    workspaces: HashMap<String, Vec<PathBuf>>,
    // by pid
    seen: HashMap<u32, Seen>,
}

impl AntigravityProvider {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            polls: 0,
            transcripts: HashMap::new(),
            workspaces: HashMap::new(),
            seen: HashMap::new(),
        }
    }

    fn transcript_path(&self, id: &str) -> PathBuf {
        self.root
            .join("brain")
            .join(id)
            .join(".system_generated/logs/transcript.jsonl")
    }

    fn load(&mut self, id: &str) {
        let path = self.transcript_path(id);
        let c = self.transcripts.entry(id.to_string()).or_insert_with(|| Cached { 
            tail: JsonlTail::new(&path), 
            transcript: Transcript::default(),
        });
        let Ok(read) = c.tail.read_new() else { return };
        if read.reset {
            c.transcript = Transcript::default();
        }
        for line in &read.lines {
            c.transcript.ingest(line);
        }
    }

    fn read_logs(&mut self) {
        self.workspaces.clear();
        let Ok(dir) = std::fs::read_dir(self.root.join("log")) else {
            return;
        };
        for entry in dir.flatten() {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                self.workspaces.extend(parse_log(&text));
            }
        }
    }


    /// The most recently written transcript of a coversation started in `cwd`
    fn newest_conversation(&self, cwd: &Path) -> Option<String> {
        self.workspaces
            .iter()
            .filter(|(_, dirs)| dirs.iter().any(|d| d == cwd))
            .filter_map(|(id, _)| Some((id, mtime(&self.transcript_path(id))?)))
            .max_by_key(|(_, t)| *t)
            .map(|(id, _)| id.clone())
    }

    /// Loads the transcripts written today and forgets the others
    fn scan_today(&mut self, now: DateTime<Utc>) {
        let day = local_day(now);
        self.read_logs();
        let in_use: HashSet<&String> = self.seen.values().filter_map(|s| s.conversation.as_ref()).collect();
        let keep: Vec<String> = self
            .transcripts
            .keys()
            .filter(|id| in_use.contains(id) || modified_on(&self.transcript_path(id), day))
            .cloned()
            .collect();
        self.transcripts.retain(|id, _| keep.contains(id));
        let Ok(dirs) = std::fs::read_dir(self.root.join("brain")) else {
            return;
        };
        let today: Vec<String> = dirs
            .flatten()
            .filter_map(|d| d.file_name().into_string().ok())
            .filter(|id| modified_on( &self.transcript_path(id), day))
            .collect();
        for id in today {
            self.load(&id);
        }
    }

    fn track(&mut self, running: &[&ProcInfo], now: DateTime<Utc>) {
        for p in running {
            let cwd = p.cwd.clone().expect("filtered by the caller");
            let conversation = self.newest_conversation(&cwd);
            if let Some(id) = &conversation {
                self.load(id);
            }
            let e = self.seen.entry(p.pid).or_insert(Seen {
                cwd: cwd.clone(),
                proc_start: p.start_time,
                conversation: None,
                started_at: now,
                dead_since: None,
            });
            e.cwd = cwd;
            e.conversation = conversation;
            e.dead_since = None;
        }
        for (pid, s) in self.seen.iter_mut() {
            if s.dead_since.is_none() && !running.iter().any(|p| p.pid == *pid) {
                s.dead_since = Some(now);
            }
        }
        self.seen.retain(|_, s| s.dead_since.is_none_or(|d| keep_dead(d, now)));
    }

    /// One row per process. Processes in the same dir find the same newest conversation: only the first (live before dead, then oldest)
    /// shows it
    fn sessions(&self, now: DateTime<Utc>) -> Vec<Session> {
        let mut order: Vec<(&u32, &Seen)> = self.seen.iter().collect();
        order.sort_by_key(|(pid, s)| (s.dead_since.is_some(), s.proc_start, **pid));
        let empty = Transcript::default();
        let mut claimed = HashSet::new();
        let mut sessions: Vec<Session> = order
            .into_iter()
            .map(|(pid, s)| {
                let conv = s.conversation.as_ref().filter(|id| claimed.insert((*id).clone()));
                let transcript = conv
                    .and_then(|id| self.transcripts.get(id))
                    .map(|c| &c.transcript)
                    .unwrap_or(&empty);
                
                let last_write = conv.and_then(|id| mtime(&self.transcript_path(id)));
                let status = gemini_status(s.dead_since, last_write, now);
                let id = conv.cloned().unwrap_or_else(|| format!("pid-{pid}"));
                let started = transcript.activity.first_activity.unwrap_or(s.started_at);
                into_session(
                    Agent::Antigravity, 
                    &id, 
                    Some(*pid), 
                    &s.cwd, 
                    started, 
                    status, 
                    &transcript.activity, 
                    now,
                )
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }
}

impl super::Provider for AntigravityProvider {
    fn poll(&mut self, now: DateTime<Utc>, procs: &dyn ProcessTable) -> ProviderReport {
        let found = self.root.is_dir();
        if found && self.polls.is_multiple_of(RESCAN_EVERY) {
            self.scan_today(now);
        }
        self.polls += 1;

        let all = if found { procs.all() } else { Vec::new() };
        let agy_pids: Vec<u32> = all.iter().filter(|p| is_agy(&p.argv)).map(|p| p.pid).collect();
        // Skip helper processes whose parent is itself an agy process
        let running: Vec<&ProcInfo> = all
            .iter()
            .filter(|p| agy_pids.contains(&p.pid) && !agy_pids.contains(&p.ppid) && p.cwd.is_some())
            .collect();
        self.track(&running, now);

        let day = local_day(now);
        let mut today = DayTotals::default();
        for c in self.transcripts.values() {
            today.add(&c.transcript.activity.day(day));
        }
        ProviderReport { 
            sessions: self.sessions(now), 
            today, 
            state: ProviderState {
                name: "antigravity",
                found,
                skipped_lines: self.transcripts.values().map(|c| c.transcript.skipped_lines).sum(),
            },
        }
    }
}