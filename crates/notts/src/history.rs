//! The history: one line per decision, appended, so that nothing is ever silently lost.
//! `notts log` reads it back.
//!
//! Line: `unix \t decision \t app \t severity \t key \t reason \t title` (fields escaped).

use crate::text::{escape_field, unescape_field};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

const ROTATE_AT: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub at: u64,
    pub decision: String,
    pub app: String,
    pub severity: String,
    pub key: String,
    pub reason: String,
    pub title: String,
}

pub struct History {
    path: PathBuf,
}

impl History {
    pub fn new(dir: &std::path::Path) -> History {
        History { path: dir.join("history.log") }
    }

    /// Best effort: a full disk must not stop a notification.
    pub fn append(&self, e: &Entry) {
        if let Ok(m) = std::fs::metadata(&self.path) {
            if m.len() > ROTATE_AT {
                let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
            }
        }
        let line = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            e.at,
            escape_field(&e.decision),
            escape_field(&e.app),
            escape_field(&e.severity),
            escape_field(&e.key),
            escape_field(&e.reason),
            escape_field(&e.title)
        );
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&self.path) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub fn read(&self) -> Vec<Entry> {
        let Ok(text) = std::fs::read(&self.path) else { return vec![] };
        String::from_utf8_lossy(&text)
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                if f.len() != 7 {
                    return None;
                }
                Some(Entry { at: f[0].parse().ok()?, decision: unescape_field(f[1]), app: unescape_field(f[2]), severity: unescape_field(f[3]), key: unescape_field(f[4]), reason: unescape_field(f[5]), title: unescape_field(f[6]) })
            })
            .collect()
    }
}
