//! Process inspection behind a trait, so tests and demos can fake it

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProcInfo {
    pub pid: u32,
    #[serde(default)]
    pub ppid: u32,
    // start time in clock ticks since boot (field 22 of /proc/<pid>/start>)
    pub start_time: u64,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

pub trait ProcessTable {
    fn get(&self, pid: u32) -> Option<ProcInfo>;
    fn all(&self) -> Vec<ProcInfo>;
}

/// Reads the real /proc
pub struct LinuxProcs;

impl ProcessTable for LinuxProcs {
    fn get(&self, pid: u32) -> Option<ProcInfo> {
        read_proc(Path::new("/proc"), pid)
    }

    fn all(&self) -> Vec<ProcInfo> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| read_proc(Path::new("/proc"), pid))
            .collect()
    }
}

fn read_proc(root: &Path, pid: u32) -> Option<ProcInfo> {
    let dir = root.join(pid.to_string());
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    let (ppid, start_time) = parse_stat(&stat)?;
    let argv = std::fs::read(dir.join("cmdline"))
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default();
    let cwd = std::fs::read_link(dir.join("cwd")).ok();
    Some(ProcInfo {
        pid,
        ppid,
        start_time,
        argv,
        cwd,
    })
}

/// Returns (ppid, starttime). The command name is in paretheses and may itself contain spaces or other parentheses
/// so fields are counted after the last `)` character
pub fn parse_stat(stat: &str) -> Option<(u32, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // rest starts at field 3 (state): ppid is field 4, starttime field 22
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

/// A fixed process table loaded from JSON (`--procs <file>`), for tests and demos
pub struct FakeProcs {
    pub procs: Vec<ProcInfo>,
}

impl FakeProcs {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            procs: serde_json::from_str(&std::fs::read_to_string(path)?)?,
        })
    }
}

impl ProcessTable for FakeProcs {
    fn get(&self, pid: u32) -> Option<ProcInfo> {
        self.procs.iter().find(|p| p.pid == pid).cloned()
    }

    fn all(&self) -> Vec<ProcInfo> {
        self.procs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_tricky_command_name() {
        let stat = "4242 (my (weird) cmd) S 100 4242 4242 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 2517088 1000 10";
        assert_eq!(parse_stat(stat), Some((100, 2517088)));
    }

    #[test]
    fn own_process_is_visible() {
        let me = LinuxProcs.get(std::process::id()).unwrap();
        assert!(me.start_time > 0);
        assert!(!me.argv.is_empty());
        assert!(LinuxProcs.all().iter().any(|p| p.pid == me.pid));
    }
}
