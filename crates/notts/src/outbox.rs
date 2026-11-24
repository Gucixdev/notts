//! The outbox: messages that could not be delivered (yet), one line per message and channel.
//! Appending is one `write` on an `O_APPEND` file (and fsync); rewriting (after a flush) goes
//! through a temp file and `rename`, so a kill at any point leaves either the old or the new
//! complete file. Both happen under the same advisory lock.

use crate::lock::{atomic_write, atomic_write_with, FileLock};
use crate::message::{Attachment, Message, Severity};
use crate::text::{base64, base64_decode, escape_field, unescape_field};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub msg: Message,
    pub channel: String,
    pub created: u64,
    pub reason: String,
}

impl Entry {
    pub fn key(&self) -> (String, String) {
        (self.msg.id.clone(), self.channel.clone())
    }

    fn to_line(&self) -> String {
        let m = &self.msg;
        format!(
            "v2\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            escape_field(&m.id),
            escape_field(&self.channel),
            self.created,
            m.severity.as_str(),
            escape_field(&m.app),
            escape_field(&m.tag),
            escape_field(&m.title),
            escape_field(&m.body),
            escape_field(m.link.as_deref().unwrap_or("")),
            escape_field(&self.reason),
            encode_atts(&m.attachments),
            "end"
        )
    }

    fn from_line(l: &str) -> Option<Entry> {
        let f: Vec<&str> = l.split('\t').collect();
        let (atts, v2) = match (f.first().copied(), f.len()) {
            (Some("v1"), 12) if f[11] == "end" => (String::new(), false),
            (Some("v2"), 13) if f[12] == "end" => (f[11].to_string(), true),
            _ => return None,
        };
        let _ = v2;
        let mut msg = Message::new(unescape_field(f[7])).body(unescape_field(f[8])).app(unescape_field(f[5])).tag(unescape_field(f[6])).severity(Severity::parse(f[4])?).link(unescape_field(f[9])).with_id(unescape_field(f[1]));
        msg.attachments = decode_atts(&atts)?;
        Some(Entry { msg, channel: unescape_field(f[2]), created: f[3].parse().ok()?, reason: unescape_field(f[10]) })
    }
}

/// `name.path.data` in base64 each, items joined by `,`; `-` when there are none.
fn encode_atts(a: &[Attachment]) -> String {
    if a.is_empty() {
        return "-".into();
    }
    a.iter().map(|x| format!("{}.{}.{}", base64(x.name.as_bytes()), base64(x.path.as_deref().unwrap_or("").as_bytes()), base64(&x.data))).collect::<Vec<_>>().join(",")
}

fn decode_atts(s: &str) -> Option<Vec<Attachment>> {
    if s == "-" || s.is_empty() {
        return Some(vec![]);
    }
    let mut v = Vec::new();
    for item in s.split(',') {
        let p: Vec<&str> = item.split('.').collect();
        if p.len() != 3 {
            return None;
        }
        let name = String::from_utf8(base64_decode(p[0])?).ok()?;
        let path = String::from_utf8(base64_decode(p[1])?).ok()?;
        v.push(Attachment { name, data: base64_decode(p[2])?, path: if path.is_empty() { None } else { Some(path) } });
    }
    Some(v)
}

pub struct Outbox {
    dir: PathBuf,
}

impl Outbox {
    pub fn new(dir: &Path) -> Outbox {
        Outbox { dir: dir.to_path_buf() }
    }
    pub fn file(&self) -> PathBuf {
        self.dir.join("outbox")
    }
    fn lock(&self) -> io::Result<FileLock> {
        std::fs::create_dir_all(&self.dir)?;
        FileLock::exclusive(&self.dir.join("outbox.lock"))
    }

    pub fn append(&self, entries: &[Entry]) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let _l = self.lock()?;
        let mut data = String::new();
        // a previous torn write may have left no newline: start on a fresh line
        if let Ok(b) = std::fs::read(self.file()) {
            if !b.is_empty() && b.last() != Some(&b'\n') {
                data.push('\n');
            }
        }
        for e in entries {
            data.push_str(&e.to_line());
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(self.file())?;
        f.write_all(data.as_bytes())?;
        f.sync_data()
    }

    /// Every complete entry; `torn` counts lines that were not valid (a torn write or damage).
    pub fn load(&self) -> (Vec<Entry>, usize) {
        let Ok(b) = std::fs::read(self.file()) else { return (vec![], 0) };
        let text = String::from_utf8_lossy(&b);
        let (mut v, mut bad) = (vec![], 0);
        for l in text.lines().filter(|l| !l.is_empty()) {
            match Entry::from_line(l) {
                Some(e) => v.push(e),
                None => bad += 1,
            }
        }
        (v, bad)
    }

    pub fn count(&self) -> usize {
        self.load().0.len()
    }

    /// Remove the given (id, channel) entries; everything else, including entries appended since
    /// the caller read the file, stays.
    pub fn remove(&self, done: &[(String, String)]) -> io::Result<()> {
        self.remove_with(done, false)
    }

    #[doc(hidden)]
    pub fn remove_with(&self, done: &[(String, String)], crash_before_rename: bool) -> io::Result<()> {
        let _l = self.lock()?;
        let (all, _) = self.load();
        let keep: String = all.iter().filter(|e| !done.contains(&e.key())).map(|e| e.to_line()).collect();
        if crash_before_rename {
            atomic_write_with(&self.file(), keep.as_bytes(), true)
        } else {
            atomic_write(&self.file(), keep.as_bytes())
        }
    }

    /// Entries too old to retry are moved to `outbox.dead` (never silently dropped).
    pub fn bury(&self, dead: &[Entry]) -> io::Result<()> {
        if dead.is_empty() {
            return Ok(());
        }
        let data: String = dead.iter().map(|e| e.to_line()).collect();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(self.dir.join("outbox.dead"))?;
        f.write_all(data.as_bytes())
    }
}
