//! e2e: the fixture home (tests/fixtures/home) read through the Claude provider, with the process table faked by tests/fixtures/procs.json

use chrono::{DateTime, Utc};
use stalltop::model::{Snapshot, Status};
use stalltop::procfs::FakeProcs;
use stalltop::providers::{Roots, collect, default_providers};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const NOW: &str = "2026-09-26T12:00:00Z";

fn utc(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Copies the fixture home and pins every mtime, because "today" is computed from mtimes
fn home() -> tempfile::TempDir {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let dest = to.join(e.file_name());
            if e.path().is_dir() {
                copy(&e.path(), &dest);
            } else {
                std::fs::copy(e.path(), &dest).unwrap();
                let at = if dest.extension().is_some_and(|x| x == "json") && dest.to_string_lossy().contains("chats") {
                    utc("2026-09-26T11:50:00Z")
                } else {
                    utc("2026-09-26T11:59:58Z")
                };
                let f = std::fs::File::options().write(true).open(&dest).unwrap();
                f.set_modified(SystemTime::from(at)).unwrap();
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    copy(&fixtures().join("home"), dir.path());
    dir
}

fn snapshot(home: &Path) -> Snapshot {
    let procs = FakeProcs::load(&fixtures().join("procs.json")).unwrap();
    let mut providers = default_providers(&Roots::resolve(Some(home)));
    collect(&mut providers, &procs, utc(NOW))
}

// tests