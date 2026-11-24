//! Persistent flood-control state in one file (`flood.state`), changed only under an exclusive
//! lock and replaced atomically. A damaged file never crashes anything: bad lines are skipped and
//! counted (that key is then "unknown", so the next message is sent once).
//!
//! Lines (tab separated, fields escaped):
//! ```text
//! G key  label last_sent held first_held     group window of a key
//! S key  state since label                   last state of a key (ok|warn|fail)
//! P key  app severity count first last channels title   held for the digest
//! F app  cooldown_until held stamps          flood guard of an app
//! D unix                                     last digest
//! ```

use crate::lock::{atomic_write, FileLock};
use crate::message::Severity;
use crate::text::{escape_field, unescape_field};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub const MAX_GROUPS: usize = 5000;
pub const MAX_STATES: usize = 5000;
pub const MAX_PENDING: usize = 2000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Group {
    pub label: String,
    pub last_sent: u64,
    /// repeats counted since the last send
    pub held: u64,
    pub first_held: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyState {
    pub state: Severity,
    pub since: u64,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub app: String,
    pub severity: Severity,
    pub count: u64,
    pub first: u64,
    pub last: u64,
    pub channels: Vec<String>,
    pub title: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flood {
    pub cooldown_until: u64,
    pub held: u64,
    pub stamps: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub groups: BTreeMap<u64, Group>,
    pub states: BTreeMap<u64, KeyState>,
    pub pending: BTreeMap<u64, Pending>,
    pub flood: BTreeMap<String, Flood>,
    pub last_digest: Option<u64>,
    /// lines that could not be read when this state was loaded
    pub damaged: usize,
}

fn hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s, 16).ok()
}

impl State {
    pub fn parse(text: &str) -> State {
        let mut st = State::default();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if st.parse_line(line).is_none() {
                st.damaged += 1;
            }
        }
        st
    }

    fn parse_line(&mut self, line: &str) -> Option<()> {
        let f: Vec<&str> = line.split('\t').collect();
        match f.first().copied()? {
            "G" if f.len() == 6 => {
                self.groups.insert(hex(f[1])?, Group { label: unescape_field(f[2]), last_sent: f[3].parse().ok()?, held: f[4].parse().ok()?, first_held: f[5].parse().ok()? });
            }
            "S" if f.len() == 5 => {
                let state = Severity::parse(f[2]).filter(|s| *s != Severity::Info)?;
                self.states.insert(hex(f[1])?, KeyState { state, since: f[3].parse().ok()?, label: unescape_field(f[4]) });
            }
            "P" if f.len() == 9 => {
                self.pending.insert(
                    hex(f[1])?,
                    Pending {
                        app: unescape_field(f[2]),
                        severity: Severity::parse(f[3])?,
                        count: f[4].parse().ok()?,
                        first: f[5].parse().ok()?,
                        last: f[6].parse().ok()?,
                        channels: f[7].split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
                        title: unescape_field(f[8]),
                    },
                );
            }
            "F" if f.len() == 5 => {
                let stamps = f[4].split(',').filter(|s| !s.is_empty()).map(|s| s.parse().ok()).collect::<Option<Vec<u64>>>()?;
                self.flood.insert(unescape_field(f[1]), Flood { cooldown_until: f[2].parse().ok()?, held: f[3].parse().ok()?, stamps });
            }
            "D" if f.len() == 2 => self.last_digest = Some(f[1].parse().ok()?),
            _ => return None,
        }
        Some(())
    }

    pub fn serialize(&self) -> String {
        let mut o = String::new();
        if let Some(d) = self.last_digest {
            o.push_str(&format!("D\t{d}\n"));
        }
        for (k, g) in &self.groups {
            o.push_str(&format!("G\t{k:016x}\t{}\t{}\t{}\t{}\n", escape_field(&g.label), g.last_sent, g.held, g.first_held));
        }
        for (k, s) in &self.states {
            o.push_str(&format!("S\t{k:016x}\t{}\t{}\t{}\n", s.state.as_str(), s.since, escape_field(&s.label)));
        }
        for (k, p) in &self.pending {
            o.push_str(&format!("P\t{k:016x}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n", escape_field(&p.app), p.severity.as_str(), p.count, p.first, p.last, p.channels.join(","), escape_field(&p.title)));
        }
        for (a, f) in &self.flood {
            let stamps: Vec<String> = f.stamps.iter().map(|s| s.to_string()).collect();
            o.push_str(&format!("F\t{}\t{}\t{}\t{}\n", escape_field(a), f.cooldown_until, f.held, stamps.join(",")));
        }
        o
    }

    /// Keep the maps bounded: drop the oldest entries beyond the caps and idle group entries.
    pub fn prune(&mut self, now: u64, group_window: u64) {
        let keep_for = group_window.max(86_400);
        self.groups.retain(|_, g| g.held > 0 || g.last_sent > now || now - g.last_sent <= keep_for);
        if self.groups.len() > MAX_GROUPS {
            let mut v: Vec<(u64, u64)> = self.groups.iter().map(|(k, g)| (g.last_sent, *k)).collect();
            v.sort_unstable();
            for (_, k) in v.iter().take(self.groups.len() - MAX_GROUPS) {
                self.groups.remove(k);
            }
        }
        if self.states.len() > MAX_STATES {
            // forget the longest-ok keys first: an ok key that is forgotten behaves as unknown, which is silent
            let mut v: Vec<(bool, u64, u64)> = self.states.iter().map(|(k, s)| (s.state != Severity::Ok, s.since, *k)).collect();
            v.sort_unstable();
            for (_, _, k) in v.iter().take(self.states.len() - MAX_STATES) {
                self.states.remove(k);
            }
        }
        self.flood.retain(|_, f| f.cooldown_until > now || !f.stamps.is_empty());
        for f in self.flood.values_mut() {
            f.stamps.retain(|t| *t <= now && now - *t < 60);
            if f.cooldown_until <= now {
                f.held = 0;
            }
        }
        self.flood.retain(|_, f| f.cooldown_until > now || !f.stamps.is_empty());
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Store {
        Store { dir: dir.to_path_buf() }
    }
    pub fn file(&self) -> PathBuf {
        self.dir.join("flood.state")
    }

    pub fn load(&self) -> State {
        match std::fs::read(self.file()) {
            Ok(b) => State::parse(&String::from_utf8_lossy(&b)),
            Err(_) => State::default(),
        }
    }

    /// Lock, load, run `f`, and (when `persist`) save atomically. A file that is not valid at all
    /// is moved aside as `flood.state.damaged` first, so it can be looked at.
    pub fn transact<R>(&self, persist: bool, f: impl FnOnce(&mut State) -> R) -> io::Result<R> {
        std::fs::create_dir_all(&self.dir)?;
        let _lock = FileLock::exclusive(&self.dir.join("flood.lock"))?;
        let mut st = self.load();
        if persist && st.damaged > 0 {
            let _ = std::fs::copy(self.file(), self.dir.join("flood.state.damaged"));
        }
        let r = f(&mut st);
        if persist {
            atomic_write(&self.file(), st.serialize().as_bytes())?;
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_line_kind() {
        let mut s = State::default();
        s.last_digest = Some(77);
        s.groups.insert(1, Group { label: "Dysk\tpełny".into(), last_sent: 5, held: 2, first_held: 4 });
        s.states.insert(2, KeyState { state: Severity::Fail, since: 9, label: "k".into() });
        s.pending.insert(3, Pending { app: "judged".into(), severity: Severity::Warn, count: 4, first: 1, last: 2, channels: vec!["a".into(), "b".into()], title: "t\nx".into() });
        s.flood.insert("app".into(), Flood { cooldown_until: 10, held: 3, stamps: vec![1, 2, 3] });
        let back = State::parse(&s.serialize());
        assert_eq!(back.serialize(), s.serialize());
        assert_eq!(back.damaged, 0);
        assert_eq!(back.groups[&1].label, "Dysk\tpełny");
    }

    #[test]
    fn damaged_lines_are_skipped_and_counted() {
        let st = State::parse("G\tzzzz\tx\t1\t1\t1\nS\t1\tfail\t5\tk\nnonsense\n\u{0}\u{0}\u{0}\nS\t2\tmaybe\t5\tk\n");
        assert_eq!(st.damaged, 4);
        assert_eq!(st.states.len(), 1);
        let all_garbage = State::parse(&"\u{fffd}x".repeat(100));
        assert_eq!(all_garbage.damaged, 1);
        assert!(all_garbage.groups.is_empty());
    }

    #[test]
    fn prune_bounds_everything() {
        let mut s = State::default();
        for i in 0..(MAX_GROUPS as u64 + 50) {
            s.groups.insert(i, Group { label: String::new(), last_sent: 1_000 + i, held: 0, first_held: 0 });
        }
        s.prune(2_000, 600);
        assert_eq!(s.groups.len(), MAX_GROUPS);
        assert!(!s.groups.contains_key(&0));
        s.flood.insert("a".into(), Flood { cooldown_until: 0, held: 9, stamps: vec![1, 2_100, 1_990] });
        s.prune(2_000, 600);
        assert_eq!(s.flood["a"].stamps, vec![1_990]);
        assert_eq!(s.flood["a"].held, 0);
    }
}
