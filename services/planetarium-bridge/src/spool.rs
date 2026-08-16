//! The bounded on-disk FIFO spool
//! (docs/services/planetarium-bridge.md § Spooling — rp unreachable).
//!
//! One JSONL file, one import request per line. Appends are append-only and
//! `fsync`ed so a crash right after an Align loses nothing; removals (a
//! replayed head, an overflow drop) rewrite the file through a temp file +
//! rename. Entries are kept as raw lines: parsing happens at replay, where a
//! corrupt line is skipped with its original line number named.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;

use tracing::{debug, error};

/// One spooled line.
#[derive(Debug, Clone)]
pub struct SpoolEntry {
    /// The raw JSONL line (no trailing newline).
    pub line: String,
    /// The 1-based line number in the file at load time; `None` for entries
    /// appended by this process (which it serialized itself).
    pub source_line_no: Option<usize>,
}

/// The durable FIFO. All operations are synchronous filesystem work — the
/// import worker owns the spool and calls from its own task.
#[derive(Debug)]
pub struct Spool {
    path: PathBuf,
    entries: VecDeque<SpoolEntry>,
    max_entries: usize,
}

impl Spool {
    /// Load the spool from `path`. A missing file is a normal first start;
    /// an unreadable file logs an error and starts empty (the service never
    /// refuses to start over its spool). Returns the spool plus the number
    /// of entries dropped right away because the file held more than
    /// `max_entries`.
    pub fn load(path: PathBuf, max_entries: usize) -> (Self, u64) {
        let entries = match std::fs::read_to_string(&path) {
            Ok(content) => content
                .lines()
                .enumerate()
                .filter(|(_, line)| !line.trim().is_empty())
                .map(|(index, line)| SpoolEntry {
                    line: line.to_owned(),
                    source_line_no: Some(index.saturating_add(1)),
                })
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no spool file; starting empty");
                VecDeque::new()
            }
            Err(e) => {
                error!(
                    path = %path.display(),
                    "spool file unreadable ({e}); starting with an empty spool"
                );
                VecDeque::new()
            }
        };
        let mut spool = Self {
            path,
            entries,
            max_entries,
        };
        let mut dropped: u64 = 0;
        while spool.entries.len() > spool.max_entries {
            let entry = spool.entries.pop_front();
            error!(
                dropped = ?entry.map(|e| e.line),
                "spool holds more than {} entries at load; dropping the oldest",
                spool.max_entries
            );
            dropped = dropped.saturating_add(1);
        }
        if dropped > 0 {
            spool.rewrite();
        }
        if !spool.entries.is_empty() {
            debug!(
                spooled = spool.entries.len(),
                path = %spool.path.display(),
                "loaded spool backlog"
            );
        }
        (spool, dropped)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn head(&self) -> Option<&SpoolEntry> {
        self.entries.front()
    }

    /// Append a line, dropping the oldest entry first when full. Returns
    /// `true` when the bound forced a drop (the caller counts it).
    pub fn append(&mut self, line: String) -> bool {
        let dropped = if self.entries.len() >= self.max_entries {
            let oldest = self.entries.pop_front();
            error!(
                dropped = ?oldest.map(|e| e.line),
                "spool full at {} entries; dropping the oldest import to admit the newest",
                self.max_entries
            );
            true
        } else {
            false
        };
        self.entries.push_back(SpoolEntry {
            line: line.clone(),
            source_line_no: None,
        });
        if dropped {
            // The head changed too — the file must be rewritten anyway.
            self.rewrite();
        } else {
            self.append_to_file(&line);
        }
        dropped
    }

    /// Remove the head (after a successful replay or a skipped corrupt
    /// line) and persist.
    pub fn pop_head(&mut self) {
        if self.entries.pop_front().is_some() {
            self.rewrite();
        }
    }

    fn append_to_file(&self, line: &str) {
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| {
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
                file.sync_all()
            });
        if let Err(e) = result {
            // The in-memory queue still holds the entry; only the
            // crash-durability of this one append is degraded.
            error!(path = %self.path.display(), "spool append failed: {e}");
        }
    }

    fn rewrite(&self) {
        let tmp = self.path.with_extension("jsonl.tmp");
        // One writable handle for write + fsync: syncing through a separate
        // read-only open fails on Windows (FlushFileBuffers needs write
        // access). std::fs::rename replaces an existing destination on
        // every supported platform.
        let result = std::fs::File::create(&tmp)
            .and_then(|mut file| {
                for entry in &self.entries {
                    file.write_all(entry.line.as_bytes())?;
                    file.write_all(b"\n")?;
                }
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = result {
            error!(path = %self.path.display(), "spool rewrite failed: {e}");
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn temp_spool(max: usize) -> (tempfile::TempDir, Spool) {
        let dir = tempfile::TempDir::new().unwrap();
        let (spool, dropped) = Spool::load(dir.path().join("spool.jsonl"), max);
        assert_eq!(dropped, 0);
        (dir, spool)
    }

    #[test]
    fn a_missing_file_loads_empty() {
        let (_dir, spool) = temp_spool(10);
        assert!(spool.is_empty());
    }

    #[test]
    fn appends_survive_a_reload() {
        let (dir, mut spool) = temp_spool(10);
        spool.append("a".into());
        spool.append("b".into());
        let (reloaded, dropped) = Spool::load(dir.path().join("spool.jsonl"), 10);
        assert_eq!(dropped, 0);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.head().unwrap().line, "a");
        assert_eq!(reloaded.head().unwrap().source_line_no, Some(1));
    }

    #[test]
    fn the_bound_drops_the_oldest() {
        let (dir, mut spool) = temp_spool(2);
        assert!(!spool.append("a".into()));
        assert!(!spool.append("b".into()));
        assert!(spool.append("c".into()));
        assert_eq!(spool.len(), 2);
        assert_eq!(spool.head().unwrap().line, "b");
        // The drop persisted, too.
        let (reloaded, _) = Spool::load(dir.path().join("spool.jsonl"), 2);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.head().unwrap().line, "b");
    }

    #[test]
    fn pop_head_persists() {
        let (dir, mut spool) = temp_spool(10);
        spool.append("a".into());
        spool.append("b".into());
        spool.pop_head();
        assert_eq!(spool.head().unwrap().line, "b");
        let (reloaded, _) = Spool::load(dir.path().join("spool.jsonl"), 10);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.head().unwrap().line, "b");
    }

    #[test]
    fn load_keeps_corrupt_lines_for_replay_to_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("spool.jsonl");
        std::fs::write(&path, "{\"ok\":1}\nnot json\n\n{\"ok\":2}\n").unwrap();
        let (spool, dropped) = Spool::load(path, 10);
        assert_eq!(dropped, 0);
        // The blank line is dropped; the corrupt line is kept with its
        // original file line number so replay can name it.
        assert_eq!(spool.len(), 3);
        let numbers: Vec<_> = spool.entries.iter().map(|e| e.source_line_no).collect();
        assert_eq!(numbers, vec![Some(1), Some(2), Some(4)]);
    }

    #[test]
    fn an_oversized_file_is_trimmed_at_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("spool.jsonl");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let (spool, dropped) = Spool::load(path.clone(), 2);
        assert_eq!(dropped, 1);
        assert_eq!(spool.len(), 2);
        assert_eq!(spool.head().unwrap().line, "b");
        let (reloaded, dropped) = Spool::load(path, 2);
        assert_eq!(dropped, 0);
        assert_eq!(reloaded.len(), 2);
    }
}
