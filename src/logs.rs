use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Utc};

use crate::agent;
use crate::beads::Bead;

pub const TAIL_BYTES: u64 = 1024 * 1024;
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

pub fn dir(repo: &Path) -> PathBuf {
    repo.join(".sulla").join("logs")
}

pub fn bead_of(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".log")?;
    let stem = stem.strip_suffix(".app").unwrap_or(stem);
    let stem = agent::reviewed_bead(stem).unwrap_or(stem);
    (!stem.is_empty()).then_some(stem)
}

pub struct Entry {
    pub name: String,
    pub modified: SystemTime,
}

pub fn stale<'a>(
    entries: &'a [Entry],
    beads: &[Bead],
    now: DateTime<Utc>,
    retention_days: i64,
    listed_at: SystemTime,
) -> Vec<&'a str> {
    let expired = |b: &Bead| {
        b.status == "closed"
            && b.closed_at.is_some_and(|t| {
                Duration::try_days(retention_days).is_some_and(|keep| now - t > keep)
            })
    };
    entries
        .iter()
        .filter(|e| e.modified < listed_at)
        .filter(|e| {
            bead_of(&e.name).is_some_and(|id| match beads.iter().find(|b| b.id == id) {
                Some(b) => expired(b),
                None => true,
            })
        })
        .map(|e| e.name.as_str())
        .collect()
}

fn entries(dir: &Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            Some(Entry {
                name: e.file_name().into_string().ok()?,
                modified: e.metadata().and_then(|m| m.modified()).ok()?,
            })
        })
        .collect()
}

pub fn prune(dir: &Path, beads: &[Bead], retention_days: i64, listed_at: SystemTime) {
    let found = entries(dir);
    let doomed = stale(&found, beads, Utc::now(), retention_days, listed_at);
    for name in &doomed {
        if let Err(e) = std::fs::remove_file(dir.join(name)) {
            tracing::warn!("pruning log {name}: {e}");
        }
    }
    if !doomed.is_empty() {
        tracing::info!("pruned {} agent logs", doomed.len());
    }
}

pub fn remove_bead(dir: &Path, id: &str) {
    for e in entries(dir) {
        if bead_of(&e.name) == Some(id)
            && let Err(err) = std::fs::remove_file(dir.join(&e.name))
        {
            tracing::warn!("removing log {}: {err}", e.name);
        }
    }
}

pub fn read_tail(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut buf = Vec::new();
    if len <= max {
        f.read_to_end(&mut buf)?;
        return Ok(buf);
    }
    f.seek(SeekFrom::Start(len - max - 1))?;
    f.take(max + 1).read_to_end(&mut buf)?;
    let cut = buf.iter().position(|&b| b == b'\n').map_or(1, |i| i + 1);
    buf.drain(..cut);
    Ok(buf)
}

pub struct Rotating {
    path: PathBuf,
    file: File,
    len: u64,
    max: u64,
}

fn previous(path: &Path) -> PathBuf {
    let mut s = OsString::from(path.as_os_str());
    s.push(".1");
    PathBuf::from(s)
}

fn append(path: &Path) -> io::Result<(File, u64)> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let len = file.metadata()?.len();
    Ok((file, len))
}

impl Rotating {
    pub fn open(path: PathBuf, max: u64) -> io::Result<Self> {
        let (file, len) = append(&path)?;
        Ok(Self {
            path,
            file,
            len,
            max,
        })
    }

    fn full(&self, len: u64, incoming: usize) -> bool {
        len > 0 && len + incoming as u64 > self.max
    }

    fn rotate(&mut self, incoming: usize) -> io::Result<()> {
        self.file.flush()?;
        let on_disk = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if self.full(on_disk, incoming) {
            std::fs::rename(&self.path, previous(&self.path))?;
        }
        (self.file, self.len) = append(&self.path)?;
        Ok(())
    }
}

impl Write for Rotating {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.full(self.len, buf.len()) {
            self.rotate(buf.len())?;
        }
        let n = self.file.write(buf)?;
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sulla-logs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn bead(id: &str, status: &str, closed_at: Option<DateTime<Utc>>) -> Bead {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "title": id,
            "status": status,
            "created_at": null,
            "closed_at": closed_at,
        }))
        .unwrap()
    }

    #[test]
    fn bead_of_strips_known_suffixes() {
        assert_eq!(bead_of("x-1.log"), Some("x-1"));
        assert_eq!(bead_of("x-1~review.log"), Some("x-1"));
        assert_eq!(bead_of("x-1.app.log"), Some("x-1"));
        assert_eq!(bead_of("x-1.2.log"), Some("x-1.2"));
        assert_eq!(bead_of("x-1.2.app.log"), Some("x-1.2"));
        assert_eq!(bead_of("notes.txt"), None);
        assert_eq!(bead_of(".log"), None);
    }

    #[test]
    fn stale_picks_expired_and_orphaned_logs() {
        let now = Utc::now();
        let listed_at = SystemTime::now();
        let old = listed_at - StdDuration::from_secs(60);
        let beads = vec![
            bead("open-1", "open", None),
            bead("recent-1", "closed", Some(now - Duration::days(1))),
            bead("old-1", "closed", Some(now - Duration::days(10))),
            bead("undated-1", "closed", None),
        ];
        let entries: Vec<Entry> = [
            "open-1.log",
            "open-1~review.log",
            "recent-1.log",
            "old-1.log",
            "old-1~review.log",
            "old-1.app.log",
            "undated-1.log",
            "gone-1.log",
            "README",
        ]
        .into_iter()
        .map(|n| Entry {
            name: n.into(),
            modified: old,
        })
        .chain([Entry {
            name: "new-1.log".into(),
            modified: listed_at + StdDuration::from_secs(1),
        }])
        .collect();
        let mut got = stale(&entries, &beads, now, 3, listed_at);
        got.sort();
        assert_eq!(
            got,
            [
                "gone-1.log",
                "old-1.app.log",
                "old-1.log",
                "old-1~review.log"
            ]
        );
    }

    #[test]
    fn stale_keeps_closed_logs_when_retention_overflows() {
        let now = Utc::now();
        let listed_at = SystemTime::now();
        let beads = vec![bead("old-1", "closed", Some(now - Duration::days(10)))];
        let entries = [Entry {
            name: "old-1.log".into(),
            modified: listed_at - StdDuration::from_secs(60),
        }];
        assert!(stale(&entries, &beads, now, i64::MAX, listed_at).is_empty());
    }

    #[test]
    fn remove_bead_deletes_only_its_logs() {
        let d = scratch("remove");
        for n in [
            "a-1.log",
            "a-1~review.log",
            "a-1.app.log",
            "a-1.2.log",
            "b-1.log",
        ] {
            std::fs::write(d.join(n), "x").unwrap();
        }
        remove_bead(&d, "a-1");
        let mut left: Vec<String> = entries(&d).into_iter().map(|e| e.name).collect();
        left.sort();
        assert_eq!(left, ["a-1.2.log", "b-1.log"]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn read_tail_returns_whole_small_file() {
        let d = scratch("tail-small");
        let p = d.join("a.log");
        std::fs::write(&p, "one\ntwo\n").unwrap();
        assert_eq!(read_tail(&p, 100).unwrap(), b"one\ntwo\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn read_tail_starts_after_first_cut_line() {
        let d = scratch("tail-cut");
        let p = d.join("a.log");
        std::fs::write(&p, "first line\nsecond\nthird\n").unwrap();
        assert_eq!(read_tail(&p, 15).unwrap(), b"second\nthird\n");
        assert_eq!(read_tail(&p, 13).unwrap(), b"second\nthird\n");
        assert_eq!(read_tail(&p, 12).unwrap(), b"third\n");
        assert_eq!(read_tail(&p, 4).unwrap(), b"");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn read_tail_fails_on_missing_file() {
        let d = scratch("tail-missing");
        assert!(read_tail(&d.join("nope.log"), 10).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rotating_keeps_one_previous_file() {
        let d = scratch("rotate");
        let p = d.join("sulla.log");
        let mut w = Rotating::open(p.clone(), 10).unwrap();
        w.write_all(b"aaaaaa\n").unwrap();
        w.write_all(b"bbbbbb\n").unwrap();
        w.write_all(b"cccccc\n").unwrap();
        w.flush().unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"cccccc\n");
        assert_eq!(std::fs::read(previous(&p)).unwrap(), b"bbbbbb\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rotating_rotates_oversized_file_on_first_write() {
        let d = scratch("rotate-open");
        let p = d.join("sulla.log");
        std::fs::write(&p, "0123456789abc").unwrap();
        let mut w = Rotating::open(p.clone(), 10).unwrap();
        w.write_all(b"new\n").unwrap();
        w.flush().unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new\n");
        assert_eq!(std::fs::read(previous(&p)).unwrap(), b"0123456789abc");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
