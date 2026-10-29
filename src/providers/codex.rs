//! Codex CLI: liveness from running processes, activity from the rollout files in
//! `sessions/YYYY/MM/DD/rollout-*.jsonl`. See docs/DATA-SOURCES.md. Codex does
//! not persist approval prompts, so sessions are working / idle / limited / ended

/// Subcommands that do not runa session in the process' directory
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
            input: n("input_tokens")
            cached: n("cached_input_tokens")
            cache_write: n("cache_write_input_tokens")
            output: n("output_tokens")
        })
    }

    fn total(&self) -> u64 {
        self.input + self.output
    }

    // 
}