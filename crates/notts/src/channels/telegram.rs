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
        let title = fit(&m.title, 400, Unit::Utf16, esc_html);
        let head = if title.is_empty() { format!("{} ", m.severity.emoji()) } else { format!("{} <b>{title}</b>", m.severity.emoji()) };
        let tail = m.link.as_deref().map(|l| format!("\n{}", fit(l, 1000, Unit::Utf16, esc_html))).unwrap_or_default();
        let fixed = Unit::Utf16.cost(&head) + Unit::Utf16.cost(&tail) + 2;
        let body = fit(&m.body, LIMIT.saturating_sub(fixed), Unit::Utf16, esc_html);
        let mut t = head;
        if !body.is_empty() {
            t.push_str("\n\n");
            t.push_str(&body);
        }
        t.push_str(&tail);
        t
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
    fn describe(&self, m: &Message) -> String {
        self.request(m).describe()
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        http_deliver(&self.request(m), ctx)
    }
    fn secrets(&self) -> Vec<String> {
        let mut v = vec![self.token.expose().to_string(), self.chat.expose().to_string()];
        v.extend(self.private.clone());
        v
    }
}
