//! Submission counter backing `/stats` (spec §9).
//!
//! The counter is **append-only and file-backed** so it survives restarts of
//! the site — the broker's e2e tests assert "the submit button was clicked
//! exactly once per payment, including across broker restarts" (SPEC.md §9).
//!
//! A record contains *only* the mode and a timestamp. Submitted form values
//! are never read, never logged and never persisted here: on this site, as
//! everywhere in this repo, card fields are radioactive (AGENTS.md rule 1).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::mode::Mode;

/// One accepted submission. Deliberately carries no form values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionRecord {
    /// Unix timestamp in seconds.
    pub ts: u64,
    /// Wire name of the mode the submission ran under.
    pub mode: String,
}

/// Point-in-time view of the counter, as served by `GET /stats`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StatsSnapshot {
    /// Total accepted submissions, cumulative across restarts.
    pub total: u64,
    /// Per-mode totals, keyed by wire name.
    pub by_mode: BTreeMap<String, u64>,
}

#[derive(Debug)]
struct Inner {
    total: u64,
    by_mode: BTreeMap<String, u64>,
    file: File,
}

/// Append-only, restart-surviving count of accepted submissions.
#[derive(Debug)]
pub struct SubmissionCounter {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl SubmissionCounter {
    /// Open (or create) the counter file and hydrate the counts from it.
    ///
    /// Lines that do not parse are skipped: a crash can leave a partial tail
    /// and the mock site must still come up for the next test run.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let (total, by_mode) = hydrate(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            inner: Mutex::new(Inner {
                total,
                by_mode,
                file,
            }),
        })
    }

    /// Path of the append-only record file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record one accepted submission and bump the counters.
    pub fn record(&self, mode: Mode) -> io::Result<()> {
        let record = SubmissionRecord {
            ts: unix_now()?,
            mode: mode.as_str().to_string(),
        };
        let mut line = serde_json::to_string(&record).map_err(io::Error::other)?;
        line.push('\n');

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("submission counter mutex poisoned"))?;
        inner.file.write_all(line.as_bytes())?;
        inner.file.flush()?;
        inner.total += 1;
        *inner.by_mode.entry(record.mode).or_insert(0) += 1;
        Ok(())
    }

    /// Total accepted submissions.
    pub fn total(&self) -> u64 {
        self.inner.lock().map(|inner| inner.total).unwrap_or(0)
    }

    /// Accepted submissions for one mode.
    pub fn by_mode(&self, mode: Mode) -> u64 {
        self.inner
            .lock()
            .map(|inner| inner.by_mode.get(mode.as_str()).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Snapshot for `GET /stats`.
    pub fn snapshot(&self) -> StatsSnapshot {
        self.inner
            .lock()
            .map(|inner| StatsSnapshot {
                total: inner.total,
                by_mode: inner.by_mode.clone(),
            })
            .unwrap_or_default()
    }
}

fn hydrate(path: &Path) -> io::Result<(u64, BTreeMap<String, u64>)> {
    let mut total = 0;
    let mut by_mode: BTreeMap<String, u64> = BTreeMap::new();

    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, by_mode)),
        Err(error) => return Err(error),
    };

    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(record) = serde_json::from_str::<SubmissionRecord>(line.trim()) else {
            continue;
        };
        total += 1;
        *by_mode.entry(record.mode).or_insert(0) += 1;
    }

    Ok((total, by_mode))
}

fn unix_now() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| io::Error::other("system clock is before the unix epoch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "mock-checkout-stats-{}-{}-{}.jsonl",
            tag,
            std::process::id(),
            seq
        ))
    }

    #[test]
    fn counter_survives_reload() -> io::Result<()> {
        let path = temp_path("reload");

        {
            let first = SubmissionCounter::open(&path)?;
            first.record(Mode::Success)?;
            first.record(Mode::Declined)?;
            first.record(Mode::Success)?;
            assert_eq!(first.total(), 3);
        }

        // A fresh instance over the same file sees the same counts — this is
        // the "including across broker restarts" requirement.
        let second = SubmissionCounter::open(&path)?;
        assert_eq!(second.total(), 3);
        assert_eq!(second.by_mode(Mode::Success), 2);
        assert_eq!(second.by_mode(Mode::Declined), 1);
        assert_eq!(second.by_mode(Mode::Ambiguous), 0);

        second.record(Mode::Ambiguous)?;
        let third = SubmissionCounter::open(&path)?;
        assert_eq!(third.total(), 4);
        assert_eq!(third.snapshot().by_mode.get("ambiguous"), Some(&1));

        std::fs::remove_file(&path)?;
        Ok(())
    }

    #[test]
    fn record_contains_no_field_values() -> io::Result<()> {
        let path = temp_path("shape");
        let counter = SubmissionCounter::open(&path)?;
        counter.record(Mode::Success)?;

        let contents = std::fs::read_to_string(&path)?;
        let line = contents.lines().next().unwrap_or_default();
        let value: serde_json::Value = serde_json::from_str(line).map_err(io::Error::other)?;
        let object = value
            .as_object()
            .ok_or_else(|| io::Error::other("record is not a json object"))?;

        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["mode", "ts"],
            "a persisted record carries only the mode and a timestamp"
        );
        assert_eq!(object.get("mode").and_then(|m| m.as_str()), Some("success"));

        std::fs::remove_file(&path)?;
        Ok(())
    }
}
