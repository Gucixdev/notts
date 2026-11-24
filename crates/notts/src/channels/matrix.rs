//! Matrix client-server API: `PUT /_matrix/client/v3/rooms/{room}/send/m.room.message/{txnId}`
//! with a bearer token. `formatted_body` is HTML-escaped. The transaction id is derived from the
//! message id, so a retry (or an outbox flush) is idempotent on the server.

use super::*;
use crate::text::{esc_html_br, fit, pct_encode, truncate, Unit};

pub const LIMIT: usize = 16_000;

pub struct Matrix {
    hs: String,
    room: Secret,
    token: Secret,
    name: String,
}

pub fn build(o: &Opts) -> Result<Box<dyn Channel>, String> {
    let hs = o.need("homeserver")?.trim_end_matches('/').to_string();
    check_url(&hs)?;
    Ok(Box::new(Matrix { hs, room: o.value("room")?, token: o.secret("token")?, name: o.spec.name.clone() }))
}

impl Matrix {
    pub fn request(&self, m: &Message) -> Request {
        let mut plain = format!("{} {}", m.severity.emoji(), m.title);
        if !m.body.is_empty() {
            plain.push_str("\n");
            plain.push_str(&m.body);
        }
        if let Some(l) = &m.link {
            plain.push('\n');
            plain.push_str(l);
        }
        let plain = truncate(&plain, LIMIT, Unit::Chars);
        let html = {
            let mut h = format!("{} <b>{}</b>", m.severity.emoji(), fit(&m.title, 400, Unit::Chars, esc_html_br));
            if !m.body.is_empty() {
                h.push_str("<br/>");
                h.push_str(&fit(&m.body, LIMIT, Unit::Chars, esc_html_br));
            }
            if let Some(l) = &m.link {
                h.push_str("<br/>");
                h.push_str(&fit(l, 1000, Unit::Chars, esc_html_br));
            }
            h
        };
        let payload = serde_json::json!({"msgtype": "m.text", "body": plain, "format": "org.matrix.custom.html", "formatted_body": html});
        let txn = format!("notts-{}-{}", m.id, self.name);
        Request::new("PUT", format!("{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}", self.hs, pct_encode(self.room.expose()), pct_encode(&txn)))
            .header("Authorization", format!("Bearer {}", self.token.expose()))
            .header("Content-Type", "application/json")
            .body(payload.to_string())
    }
}

impl Channel for Matrix {
    fn kind(&self) -> &'static str {
        "matrix"
    }
    fn describe(&self, m: &Message) -> String {
        self.request(m).describe()
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        http_deliver(&self.request(m), ctx)
    }
    fn secrets(&self) -> Vec<String> {
        vec![self.token.expose().to_string(), self.room.expose().to_string()]
    }
}
