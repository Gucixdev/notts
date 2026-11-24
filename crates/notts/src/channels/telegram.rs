//! Telegram Bot API `sendMessage`, HTML parse mode (the only escaping needed is `& < >`).
//! Limit: 4096 characters after parsing; counted here on the escaped text in UTF-16 units, which is stricter.

use super::*;
use crate::text::{esc_html, fit, Unit};

pub const LIMIT: usize = 4096;
const DEFAULT_API: &str = "https://api.telegram.org";

pub struct Telegram {
    api: String,
    token: Secret,
    chat: Secret,
    private: Vec<String>,
}

pub fn build(o: &Opts) -> Result<Box<dyn Channel>, String> {
    let token = o.secret("token")?;
    let chat = o.value("chat")?;
    let api = o.get("api").unwrap_or(DEFAULT_API).trim_end_matches('/').to_string();
    check_url(&api)?;
    Ok(Box::new(Telegram { api, token, chat, private: o.private_values() }))
}

impl Telegram {
    /// The message text in Telegram HTML, at most `LIMIT` units.
    pub fn render(&self, m: &Message) -> String {
        self.render_with(m, LIMIT)
    }

    fn render_with(&self, m: &Message, limit: usize) -> String {
        let title = fit(&m.title, 400, Unit::Utf16, esc_html);
        let head = if title.is_empty() { format!("{} ", m.severity.emoji()) } else { format!("{} <b>{title}</b>", m.severity.emoji()) };
        let tail = m.link.as_deref().map(|l| format!("\n{}", fit(l, 1000, Unit::Utf16, esc_html))).unwrap_or_default();
        let fixed = Unit::Utf16.cost(&head) + Unit::Utf16.cost(&tail) + 2;
        let body = fit(&m.body, limit.saturating_sub(fixed), Unit::Utf16, esc_html);
        let mut t = head;
        if !body.is_empty() {
            t.push_str("\n\n");
            t.push_str(&body);
        }
        t.push_str(&tail);
        t
    }

    /// `sendDocument`: the file with the title as its caption (plain text, 1024 chars at most).
    pub fn document_request(&self, m: &Message, a: &crate::message::Attachment) -> Request {
        let caption = crate::text::truncate(format!("{} {}", m.severity.emoji(), m.title).trim(), 1000, Unit::Utf16);
        let (ct, body) = Multipart::new(&m.id).field("chat_id", self.chat.expose()).field("caption", &caption).file("document", &a.name, &a.data).finish();
        Request::new("POST", format!("{}/bot{}/sendDocument", self.api, self.token.expose())).header("Content-Type", ct).body(body)
    }

    pub fn request(&self, m: &Message) -> Request {
        let payload = serde_json::json!({
            "chat_id": self.chat.expose(),
            "text": self.render(m),
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        Request::new("POST", format!("{}/bot{}/sendMessage", self.api, self.token.expose())).header("Content-Type", "application/json").body(payload.to_string())
    }
}

impl Channel for Telegram {
    fn kind(&self) -> &'static str {
        "telegram"
    }
    fn capacity(&self) -> Option<Capacity> {
        Some(Capacity { limit: LIMIT, max_parts: 10 })
    }
    fn cost(&self, m: &Message) -> usize {
        Unit::Utf16.cost(&self.render_with(m, 1 << 30))
    }
    fn files(&self) -> Option<FileCaps> {
        Some(FileCaps { max_bytes: 20 * 1024 * 1024, with_text: false })
    }
    fn describe(&self, m: &Message) -> String {
        match m.attachments.first() {
            Some(a) => self.document_request(m, a).describe(),
            None => self.request(m).describe(),
        }
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        match m.attachments.first() {
            Some(a) => http_deliver(&self.document_request(m, a), ctx),
            None => http_deliver(&self.request(m), ctx),
        }
    }
    fn secrets(&self) -> Vec<String> {
        let mut v = vec![self.token.expose().to_string(), self.chat.expose().to_string()];
        v.extend(self.private.clone());
        v
    }
}
