//! Codex CLI: liveness from running processes, activity from the rollout files in
//! `sessions/YYYY/MM/DD/rollout-*.jsonl`. See docs/DATA-SOURCES.md. Codex does
//! not persist approval prompts, so sessions are working / idle / limited / ended

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::SystemTime,
};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::Value;

use crate::{
    limits::LimitHit,
    model::{Activity, Agent, DayTotals, Event, EventKind, ProviderState, Session, TokenUsage, into_session},
    pricing::cost_usd,
    procfs::{ProcInfo, ProcessTable},
    providers::{ProviderReport, RESCAN_EVERY, clip, local_day, modified_on, mtime},
    status::{codex_status, keep_dead},
    tail::JsonlTail,
};

/// Subcommands that do not run a session in the process' directory
const NOT_SESSIONS: [&str; 12] = [
    "login",
    "logout",
    "mcp",
    "mcp-server",
    "plugin",
    "app-server",
    "app",
    "remote-control",
    "completion",
    "update",
    "tcp-tunnel",
    "agents",
];

// Codex counters are cumulative for the thread. Each update is turned into the
// difference with the previous one, so repeated or duplicated records (a `token_count`)
// event and a `token_usage_record` for the same response) bill nothing
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counters {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
}

impl Counters {
    fn parse(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        let n = |k: &str| v[k].as_i64().unwrap_or(0).max(0) as u64;
        Some(Self {
            input: n("input_tokens"),
            cached: n("cached_input_tokens"),
            cache_write: n("cache_write_input_tokens"),
            output: n("output_tokens"),
        })
    }

    fn total(&self) -> u64 {
        self.input + self.output
    }

    // `self` minus `prev`. When the total went down the thread was reset (or the file rewritten): `self` counts from 0
    fn since(&self, prev: &Counters) -> Counters {
        if self.total() < prev.total() {
            return *self;
        }
        Counters {
            input: self.input.saturating_sub(prev.input),
            cached: self.cached.saturating_sub(prev.cached),
            cache_write: self.cache_write.saturating_sub(prev.cache_write),
            output: self.output.saturating_sub(prev.output),
        }
    }

    /// `input_tokens` includes cached input and cache writes. Reasoning tokens are part of `output_tokens`
    fn usage(&self) -> TokenUsage {
        TokenUsage {
            input: self.input.saturating_sub(self.cached + self.cache_write),
            output: self.output,
            cache_write_5m: self.cache_write,
            cache_write_1h: self.cached,
            ..Default::default()
        }
    }
}

/// Everything stalltop derives from one rollout file
#[derive(Debug, Default)]
pub struct Rollout {
    pub thread_id: Option<String>,
    /// Set for subagent threads
    pub parent: Option<String>,
    pub cwd: Option<PathBuf>,
    pub started_at: Option<DateTime<Utc>>,
    pub activity: Activity,
    /// Set while the last rate-limit snapshot says a limit is reached
    pub limit: Option<LimitHit>,
    counters: Counters,
    pub skipped_lines: u64,
}

impl Rollout {
    /// One line: `{"timestamp":...,"type":...,"payload":{...}}`
    pub fn ingest(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            self.skipped_lines += 1;
            return;
        };
        let Some(ts) = v["timestamp"].as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok()) else {
            self.skipped_lines += 1;
            return;
        };
        let p = &v["payload"];
        match v["type"].as_str() {
            Some("session_meta") => {
                self.thread_id = p["id"].as_str().map(String::from);
                self.parent = p["parent_thread_id"].as_str().map(String::from);
                self.cwd = p["cwd"].as_str().map(PathBuf::from);
                self.started_at = p["timestamp"].as_str().and_then(|s| s.parse().ok()).or(Some(ts));
            }
            Some("turn_context") => {
                if let Some(m) = p["model"].as_str() {
                    self.activity.model = Some(m.to_string());
                }
                self.activity.touch(ts);
            }
            Some("token_usage_record") => self.add_counters(ts, &p["thread_token_usage"]),
            Some("event_msg") if p["type"] == "token_count" => {
                self.add_counters(ts, &p["info"]["total_token_usage"]);
                if p["rate_limits"].is_object() {
                    self.limit = limit_hit(&p["rate_limits"]);
                }
            }

            Some("response_item") => self.response_item(ts, p),
            Some(_) => self.activity.touch(ts),
            None => self.skipped_lines += 1,
        }
    }

    fn add_counters(&mut self, ts: DateTime<Utc>, v: &Value) {
        let Some(now) = Counters::parse(v) else {
            self.activity.touch(ts);
            return;
        };
        let delta = now.since(&self.counters);
        self.counters = now;
        if delta.total() == 0 {
            self.activity.touch(ts);
            return;
        }

        let usage = delta.usage();
        let cost = self.activity.model.as_deref().and_then(|m| cost_usd(m, &usage));
        self.activity.add_usage(ts, &usage, cost);
        // A new response means the limit no longer blocks the session
        self.limit = None;
    }

    fn response_item(&mut self, ts: DateTime<Utc>, p: &Value) {
        let a = &mut self.activity;
        match p["type"].as_str() {
            Some("message") => {
                let text = message_text(&p["content"]);
                let kind = match p["role"].as_str() {
                    Some("user") if is_prompt(&text) => EventKind::Prompt,
                    Some("assistant") if !text.trim().is_empty() => EventKind::Reply,
                    _ => return a.touch(ts),
                };
                a.push_event(Event {
                    at: ts,
                    kind,
                    text: clip(&text, 120),
                });
            }
            Some("function_call") => {
                let args: Value = p["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                let name = p["name"].as_str().unwrap_or("tool");
                let desc = match (
                    command(&args["command"]).or(command(&args["cmd"])),
                    args["path"].as_str(),
                ) {
                    (Some(c), _) => clip(&format!("{name} {c}"), 60),
                    (None, Some(f)) => format!("{name} {}", base(f)),
                    _ => name.to_string(),
                };
                tool(a, ts, name, desc);
            }
            Some("local_shell_call") => {
                let desc = command(&p["action"]["command"])
                    .map(|c| clip(&format!("shell {c}"), 60))
                    .unwrap_or_else(|| "shell".into());
                tool(a, ts, "shell", desc);
            }
            Some("custom_tool_call") => {
                let name = p["name"].as_str().unwrap_or("tool");
                let input = p["input"].as_str().unwrap_or_default();
                let files = patched_files(input);
                let desc = match files.first() {
                    Some(f) => format!("{name} {}", base(f)),
                    None => name.to_string(),
                };
                a.files_touched.extend(files.into_iter().map(PathBuf::from));
                tool(a, ts, name, desc);
            }
            Some("web_search_all") => tool(a, ts, "web_search", "web_search".into()),
            _ => a.touch(ts),
        }
    }
}

fn tool(a: &mut Activity, ts: DateTime<Utc>, name: &str, desc: String) {
    *a.tool_counts.entry(name.to_string()).or_default() += 1;
    a.last_tool = Some(desc.clone());
    a.push_event(Event {
        at: ts,
        kind: EventKind::Tool,
        text: desc,
    });
}

/// `["bash","-lc","cargo test"]` -> `cargo test`; a plain string as is
fn command(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [shell, flag, script] if is_shell(shell) && flag.starts_with('-') => Some(script.to_string()),
                [] => None,
                p => Some(p.join(" ")),
            }
        }
        _ => None,
    }
}

fn is_shell(s: &str) -> bool {
    matches!(base(s).as_str(), "bash" | "sh" | "zsh" | "dash")
}

fn base(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Files named in an `apply_patch` input (`*** Add File: <path>`, ...)
pub fn patched_files(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|h| l.strip_prefix(h))
        })
        .map(|f| f.trim().to_string())
        .collect()
}

/// Text of `input_text` / `output_text` parts
fn message_text(c: &Value) -> String {
    c.as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// Codex also stores injected context as user messages: the env block, AGENTS.md and other instructions, all
/// starting with a tag or a heading
fn is_prompt(text: &str) -> bool {
    let t = text.trim_start();
    !t.is_empty() && !t.starts_with('<') && !t.starts_with("# AGENTS.md")
}

/// A rate-limit snapshot: Some when a windows is used up or the server says a limit is reached. The kind comes
/// from the window length
pub fn limit_hit(rl: &Value) -> Option<LimitHit> {
    let windows = [&rl["primary"], &rl["secondary"]];
    let full = windows
        .iter()
        .filter(|w| w["used_percent"].as_f64().is_some_and(|p| p >= 100.0))
        .max_by_key(|w| w["resets_at"].as_i64().unwrap_or(0));
    let reached = rl["rate_limit_reached_type"].is_string();
    if full.is_none() && !reached {
        return None;
    }
    let w = full.copied().unwrap_or(&Value::Null);
    let kind = match w["window_minutes"].as_i64() {
        Some(300) => "5-hour".to_string(),
        Some(10_080) => "weekly".to_string(),
        Some(m) if m > 0 && m % 1440 == 0 => format!("{}-day", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{}-hour", m / 60),
        _ => "usage".to_string(),
    };
    let reset_at = w["resets_at"].as_i64().and_then(|s| Utc.timestamp_opt(s, 0).single());
    Some(LimitHit { kind, reset_at })
}

// The native binary (`codex`), or the npm wrapper (`node .../bin/codex.js`)
pub fn is_codex(argv: &[String]) -> bool {
    let Some(first) = argv.first() else { return false };
    let (cmd, rest) = if base(first) == "codex" {
        (true, &argv[1..])
    } else if super::gemini::is_runtime(&base(first))
        && argv
            .get(1)
            .is_some_and(|a| matches!(base(a).as_str(), "codex" | "codex.js") || a.contains("@openai/codex"))
    {
        (true, &argv[2..])
    } else {
        (false, &argv[..0])
    };
    cmd && rest.first().is_none_or(|a| !NOT_SESSIONS.contains(&a.as_str()))
}

/// `sessions/YYYY/MM/DD` for a local date: where Codex creates new rollouts
pub fn day_dir(sessions: &Path, day: NaiveDate) -> PathBuf {
    sessions.join(day.format("%Y/%m/%d").to_string())
}

fn is_rollout(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
}

fn rollouts_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries.flatten().map(|e| e.path()).filter(|p| is_rollout(p)).collect()
}

/// Every rollout under `sessions/`, at any depth. Compressed (`.zst`) rollouts are old ones Codex archived:
/// they are not read
fn all_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for p in entries.flatten().map(|e| e.path()) {
        if p.is_dir() {
            all_rollouts(&p, out);
        } else if is_rollout(&p) {
            out.push(p);
        }
    }
}

struct Cached {
    modified: SystemTime,
    tail: JsonlTail,
    rollout: Rollout,
}

struct Seen {
    cwd: PathBuf,
    proc_start: u64,
    rollout: Option<PathBuf>,
    started_at: DateTime<Utc>,
    dead_since: Option<DateTime<Utc>>,
}

pub struct CodexProvider {
    root: PathBuf,
    polls: u64,
    rollouts: HashMap<PathBuf, Cached>,
    /// By pid
    seen: HashMap<u32, Seen>,
}

impl CodexProvider {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            polls: 0,
            rollouts: HashMap::new(),
            seen: HashMap::new(),
        }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Reads the lines appended since the last call
    fn load(&mut self, path: &Path) {
        let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            return;
        };
        let c = self.rollouts.entry(path.to_path_buf()).or_insert_with(|| Cached {
            modified,
            tail: JsonlTail::new(path),
            rollout: Rollout::default(),
        });
        if c.modified == modified && c.rollout.thread_id.is_some() {
            return;
        }
        let Ok(read) = c.tail.read_new() else { return };
        if read.reset {
            c.rollout = Rollout::default();
        }
        for line in &read.lines {
            c.rollout.ingest(line);
        }
        c.modified = modified;
    }

    /// Loads every rollout modified today and forgets those neither in use nor from today
    fn scan_today(&mut self, now: DateTime<Utc>) {
        let day = local_day(now);
        let in_use: HashSet<PathBuf> = self.in_use();
        self.rollouts.retain(|p, _| in_use.contains(p) || modified_on(p, day));
        let mut files = Vec::new();
        all_rollouts(&self.sessions_dir(), &mut files);
        for p in files.into_iter().filter(|p| modified_on(p, day)) {
            self.load(&p);
        }
    }

    /// Rollouts shown in a row: the session's own and its subagents'
    fn in_use(&self) -> HashSet<PathBuf> {
        let ids: HashSet<&str> = self
            .seen
            .values()
            .filter_map(|s| s.rollout.as_ref())
            .filter_map(|p| self.rollouts.get(p)?.rollout.thread_id.as_deref())
            .collect();
        self.rollouts
            .iter()
            .filter(|(p, c)| {
                self.seen.values().any(|s| s.rollout.as_ref() == Some(*p))
                    || c.rollout.parent.as_deref().is_some_and(|id| ids.contains(id))
            })
            .map(|(p, _)| p.clone())
            .collect()
    }

    // Newest main-thread rollout started in `cwd`
    fn newest_for(&self, cwd: &Path) -> Option<PathBuf> {
        self.rollouts
            .iter()
            .filter(|(_, c)| c.rollout.parent.is_none() && c.rollout.cwd.as_deref() == Some(cwd))
            .max_by_key(|(_, c)| c.modified)
            .map(|(p, _)| p.clone())
    }

    fn track(&mut self, running: &[&ProcInfo], now: DateTime<Utc>) {
        for p in running {
            let cwd = p.cwd.clone().expect("filtered_by_the_caller");
            let rollout = self.newest_for(&cwd);
            let started_at = rollout
                .as_ref()
                .and_then(|r| self.rollouts.get(r))
                .and_then(|c| c.rollout.started_at)
                .unwrap_or(now);
            let e = self.seen.entry(p.pid).or_insert(Seen {
                cwd: cwd.clone(),
                proc_start: p.start_time,
                rollout: None,
                started_at,
                dead_since: None,
            });
            e.cwd = cwd;
            e.rollout = rollout;
            e.dead_since = None;
        }
        for (pid, s) in self.seen.iter_mut() {
            if s.dead_since.is_none() && !running.iter().any(|p| p.pid == *pid) {
                s.dead_since = Some(now);
            }
        }
        self.seen.retain(|_, s| s.dead_since.is_none_or(|d| keep_dead(d, now)));
    }

    // One row per process, like Gemini: processes sharing a directory share its newest rollout, which only the first
    // (live before dead, then oldest) shows
    fn sessions(&self, now: DateTime<Utc>) -> Vec<Session> {
        let mut order: Vec<(&u32, &Seen)> = self.seen.iter().collect();
        order.sort_by_key(|(pid, s)| (s.dead_since.is_some(), s.proc_start, **pid));
        let empty = Rollout::default();
        let mut claimed = HashSet::new();
        let mut sessions: Vec<Session> = order
            .into_iter()
            .map(|(pid, s)| {
                let owned = s
                    .rollout
                    .as_ref()
                    .filter(|p| claimed.insert((*p).clone()))
                    .and_then(|p| self.rollouts.get(p));
                let r = owned.map(|c| &c.rollout).unwrap_or(&empty);
                let mut activity = r.activity.clone();
                let mut last_write = s.rollout.as_deref().filter(|_| owned.is_some()).and_then(mtime);
                if let Some(id) = r.thread_id.as_deref() {
                    for (path, c) in &self.rollouts {
                        if c.rollout.parent.as_deref() == Some(id) {
                            activity.merge_usage(&c.rollout.activity);
                            last_write = last_write.max(mtime(path));
                        }
                    }
                }
                let status = codex_status(s.dead_since, r.limit.as_ref(), last_write, now);
                let id = r.thread_id.clone().unwrap_or_else(|| format!("pid-{pid}"));
                into_session(
                    Agent::Codex,
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

impl super::Provider for CodexProvider {
    fn poll(&mut self, now: DateTime<Utc>, procs: &dyn ProcessTable) -> ProviderReport {
        let found = self.root.is_dir();
        if found && self.polls.is_multiple_of(RESCAN_EVERY) {
            self.scan_today(now);
        }
        self.polls += 1;

        let all = if found { procs.all() } else { Vec::new() };
        let codex_pids: Vec<u32> = all.iter().filter(|p| is_codex(&p.argv)).map(|p| p.pid).collect();
        // The npm wrapper runs the native binary as its child: keep the parent only
        let running: Vec<&ProcInfo> = all
            .iter()
            .filter(|p| codex_pids.contains(&p.pid) && !codex_pids.contains(&p.ppid) && p.cwd.is_some())
            .collect();
        if !running.is_empty() {
            // New rollouts are created in today's directory: look there every poll, and read what the rollouts on
            // screen appended
            for p in rollouts_in(&day_dir(&self.sessions_dir(), local_day(now))) {
                self.load(&p);
            }
            for p in self.in_use() {
                self.load(&p);
            }
        }
        self.track(&running, now);

        let day = local_day(now);
        let mut today = DayTotals::default();
        for c in self.rollouts.values() {
            today.add(&c.rollout.activity.day(day));
        }
        ProviderReport {
            sessions: self.sessions(now),
            today,
            state: ProviderState {
                name: "codex",
                found,
                skipped_lines: self.rollouts.values().map(|c| c.rollout.skipped_lines).sum(),
            },
        }
    }
}
