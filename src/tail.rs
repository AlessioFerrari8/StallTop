//! Incremental reader for append-only JSONL files

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub struct TailRead {
    // Complete new lines, without the trailing newline
    pub lines: Vec<String>,
    // True when the file shrank or was replaced: the caller must drop what it derived from earlier lines, because `lines`
    // starts from the beginning again
    pub reset: bool,
}

pub struct JsonlTail {
    path: PathBuf,
    offset: u64,
    inode: Option<u64>,
    partial: Vec<u8>,
}

impl JsonlTail {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            inode: None,
            partial: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_new(&mut self) -> io::Result<TailRead> {
        let mut file = File::open(&self.path)?;
        let meta = file.metadata()?;
        let mut reset = false;
        if meta.len() < self.offset || self.inode.is_some_and(|i| i != meta.ino()) {
            self.offset = 0;
            self.partial.clear();
            reset = true;
        }
        self.inode = Some(meta.ino());
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        self.offset += buf.len() as u64;
        self.partial.extend_from_slice(&buf);

        let mut lines = Vec::new();
        let mut start = 0;
        for (i, b) in self.partial.iter().enumerate() {
            if *b == b'\n' {
                let line = String::from_utf8_lossy(&self.partial[start..i])
                    .trim_end_matches('\r')
                    .to_string();
                if !line.is_empty() {
                    lines.push(line);
                }
                start = i + 1;
            }
        }
        self.partial.drain(..start);
        Ok(TailRead { lines, reset })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_only_new_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "a\nb\npart").unwrap();
        let mut t = JsonlTail::new(&p);
        let r = t.read_new().unwrap();
        assert_eq!(r.lines, ["a", "b"]);
        assert!(!r.reset);

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "ial\nc\n").unwrap();
        assert_eq!(t.read_new().unwrap().lines, ["partial", "c"]);
        assert!(t.read_new().unwrap().lines.is_empty());
    }

    #[test]
    fn truncation_restarts_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "one\ntwo\n").unwrap();
        let mut t = JsonlTail::new(&p);
        t.read_new().unwrap();
        std::fs::write(&p, "x\n").unwrap();
        let r = t.read_new().unwrap();
        assert!(r.reset);
        assert_eq!(r.lines, ["x"]);
    }

    #[test]
    fn replaced_file_restarts_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "one\n").unwrap();
        let mut t = JsonlTail::new(&p);
        t.read_new().unwrap();
        let tmp = dir.path().join("new");
        std::fs::write(&tmp, "one\ntwo\nthree\n").unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        let r = t.read_new().unwrap();
        assert!(r.reset);
        assert_eq!(r.lines, ["one", "two", "three"]);
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(JsonlTail::new("/nonexistent/stalltop.jsonl").read_new().is_err());
    }
}
