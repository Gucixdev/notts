//! Discord incoming webhook. The webhook URL is the secret. Limit: 2000 characters.
//! `@everyone`/`@here` are defused with a zero-width space and `allowed_mentions` is empty,
//! so nothing a message contains can ping anyone.

use super::*;
use crate::text::{fit, no_esc, Unit};

pub const LIMIT: usize = 2000;

pub struct Discord {
    url: Secret,
    username: Option<String>,
}

pub fn build(o: &Opts) -> Result<Box<dyn Channel>, String> {
    let url = o.url("webhook")?;
    Ok(Box::new(Discord { url, username: o.get("username").map(String::from) }))
}

/// Insert a zero-width space after the `@` of `@everyone` and `@here` (any case).
pub fn defuse_mentions(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let lower: Vec<char> = s.chars().flat_map(|c| c.to_lowercase()).collect();
    let chars: Vec<char> = s.chars().collect();
    // lowercase may change length for exotic chars; fall back to a char-wise scan on the original
    let same = lower.len() == chars.len();
    let mut i = 0;
    while i < chars.len() {
        out.push(chars[i]);
        if chars[i] == '@' && same {
            let rest: String = lower[i + 1..].iter().take(8).collect();
            if rest.starts_with("everyone") || rest.starts_with("here") {
                out.push('\u{200b}');
            }
        }
        i += 1;
    }
    out
}

impl Discord {
    pub fn render(&self, m: &Message) -> String {
        let mut s = String::new();
        s.push_str(m.severity.emoji());
        if !m.title.is_empty() {
            s.push_str(&format!(" **{}**", defuse_mentions(&m.title)));
        }
        let tail = m.link.as_deref().map(|l| format!("\n{}", defuse_mentions(l))).unwrap_or_default();
        let fixed = Unit::Utf16.cost(&s) + Unit::Utf16.cost(&tail) + 1;
        if !m.body.is_empty() {
            s.push('\n');
            s.push_str(&fit(&defuse_mentions(&m.body), LIMIT.saturating_sub(fixed), Unit::Utf16, no_esc));
        }
        s.push_str(&tail);
        s
    }

    pub fn request(&self, m: &Message) -> Request {
        let mut p = serde_json::json!({"content": self.render(m), "allowed_mentions": {"parse": []}});
        if let Some(u) = &self.username {
            p["username"] = serde_json::Value::String(u.clone());
        }
        Request::new("POST", self.url.expose()).header("Content-Type", "application/json").body(p.to_string())
    }
}

impl Channel for Discord {
    fn kind(&self) -> &'static str {
        "discord"
    }
    fn describe(&self, m: &Message) -> String {
        self.request(m).describe()
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        http_deliver(&self.request(m), ctx)
    }
    fn secrets(&self) -> Vec<String> {
        vec![self.url.expose().to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_are_defused() {
        let d = defuse_mentions("hi @everyone and @HERE, @here! user@example.com");
        assert!(!d.contains("@everyone") && !d.to_lowercase().contains("@here"));
        assert!(d.contains("@\u{200b}everyone") && d.contains("@\u{200b}HERE"));
        assert!(d.contains("user@example.com"));
    }
}
