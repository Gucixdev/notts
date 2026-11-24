//! The message: what callers build and what every channel renders.

use crate::text::{fnv64, truncate, Unit, MARKER};
use std::sync::atomic::{AtomicU64, Ordering};

/// Cap applied to every body before any channel sees it.
pub const MAX_BODY_CHARS: usize = 100_000;
pub const MAX_TITLE_CHARS: usize = 300;

/// Ordered `Info < Ok < Warn < Fail`; rules use it for `min=`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Severity {
    Info,
    Ok,
    Warn,
    Fail,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Severity> {
        match s.to_ascii_lowercase().as_str() {
            "info" => Some(Severity::Info),
            "ok" => Some(Severity::Ok),
            "warn" | "warning" => Some(Severity::Warn),
            "fail" | "error" | "critical" => Some(Severity::Fail),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Fail => "fail",
        }
    }
    /// Emoji used by chat channels.
    pub fn emoji(self) -> &'static str {
        match self {
            Severity::Info => "\u{2139}\u{fe0f}",
            Severity::Ok => "\u{2705}",
            Severity::Warn => "\u{26a0}\u{fe0f}",
            Severity::Fail => "\u{274c}",
        }
    }
}

/// One notification. Build with [`Message::new`] and the chained setters.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Message {
    pub id: String,
    pub app: String,
    pub tag: String,
    pub title: String,
    pub body: String,
    pub severity: Severity,
    pub link: Option<String>,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fresh_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()).to_string();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed).to_string();
    let pid = std::process::id().to_string();
    format!("{:016x}", fnv64(&[&nanos, &n, &pid]))
}

impl Message {
    pub fn new(title: impl Into<String>) -> Message {
        Message { id: fresh_id(), app: String::new(), tag: String::new(), title: title.into(), body: String::new(), severity: Severity::Info, link: None }
    }
    pub fn body(mut self, v: impl Into<String>) -> Self {
        self.body = v.into();
        self
    }
    /// Body from raw bytes; invalid UTF-8 becomes U+FFFD instead of an error.
    pub fn body_bytes(mut self, v: &[u8]) -> Self {
        self.body = String::from_utf8_lossy(v).into_owned();
        self
    }
    pub fn severity(mut self, v: Severity) -> Self {
        self.severity = v;
        self
    }
    pub fn app(mut self, v: impl Into<String>) -> Self {
        self.app = v.into();
        self
    }
    pub fn tag(mut self, v: impl Into<String>) -> Self {
        self.tag = v.into();
        self
    }
    pub fn link(mut self, v: impl Into<String>) -> Self {
        let v = v.into();
        self.link = if v.is_empty() { None } else { Some(v) };
        self
    }
    /// Rebuild a message that was stored (outbox): keeps the id so retries are idempotent.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Clean-up applied by the engine: no control characters, one-line title/app/tag, capped body.
    pub fn normalized(&self) -> Message {
        let mut m = self.clone();
        m.title = truncate(&one_line(&m.title), MAX_TITLE_CHARS, Unit::Chars);
        m.app = one_line(&m.app).chars().take(64).collect();
        m.tag = one_line(&m.tag).chars().take(64).collect();
        m.body = clean(&m.body);
        if m.body.chars().count() > MAX_BODY_CHARS {
            let cut = truncate(&m.body, MAX_BODY_CHARS, Unit::Chars);
            debug_assert!(cut.ends_with(MARKER));
            m.body = cut;
        }
        m.link = m.link.map(|l| one_line(&l)).filter(|l| !l.is_empty());
        m
    }

    /// Key for the de-duplication window: app + title + body.
    pub fn dedupe_key(&self) -> u64 {
        fnv64(&[&self.app, &self.title, &self.body])
    }

    /// `title` and `body` as one text block.
    pub fn text(&self) -> String {
        match (self.title.is_empty(), self.body.is_empty()) {
            (_, true) => self.title.clone(),
            (true, false) => self.body.clone(),
            _ => format!("{}\n\n{}", self.title, self.body),
        }
    }
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| *c == '\n' || *c == '\t' || !c.is_control()).collect()
}

fn one_line(s: &str) -> String {
    clean(s).replace(['\n', '\t'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_and_parses() {
        assert!(Severity::Fail > Severity::Warn && Severity::Warn > Severity::Ok && Severity::Ok > Severity::Info);
        assert_eq!(Severity::parse("WARN"), Some(Severity::Warn));
        assert_eq!(Severity::parse("nope"), None);
    }

    #[test]
    fn normalize_strips_controls_and_caps() {
        let m = Message::new("a\nb\u{0}c").body("x\u{7}y\nz").normalized();
        assert_eq!(m.title, "a bc");
        assert_eq!(m.body, "xy\nz");
        let big = Message::new("t").body("ł".repeat(MAX_BODY_CHARS + 5)).normalized();
        assert_eq!(big.body.chars().count(), MAX_BODY_CHARS);
        assert!(big.body.ends_with(MARKER));
    }

    #[test]
    fn invalid_utf8_becomes_replacement_char() {
        let m = Message::new("t").body_bytes(&[b'o', b'k', 0xff, 0xfe, b'!']);
        assert_eq!(m.body, "ok\u{fffd}\u{fffd}!");
    }

    #[test]
    fn ids_are_unique_and_dedupe_key_ignores_id_and_severity() {
        let a = Message::new("t").body("b").app("x");
        let b = Message::new("t").body("b").app("x").severity(Severity::Fail);
        assert_ne!(a.id, b.id);
        assert_eq!(a.dedupe_key(), b.dedupe_key());
        assert_ne!(a.dedupe_key(), Message::new("t").body("b").app("y").dedupe_key());
    }
}
