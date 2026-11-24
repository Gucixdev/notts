//! Matrix client-server API: `PUT /_matrix/client/v3/rooms/{room}/send/m.room.message/{txnId}`
//! with a bearer token. `formatted_body` is HTML-escaped. The transaction id is derived from the
//! message id, so a retry (or an outbox flush) is idempotent on the server.

use super::*;
use crate::text::{esc_html_br, fit, pct_encode, truncate, Unit};

/// Per text event, counted on the plain text and on the escaped HTML, so one event stays far below
/// the 65 KB event limit of the spec.
pub const LIMIT: usize = 6_000;

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
    fn texts(&self, m: &Message, limit: usize) -> (String, String) {
        let mut plain = format!("{} {}", m.severity.emoji(), m.title);
        if !m.body.is_empty() {
            plain.push('\n');
            plain.push_str(&m.body);
        }
        if let Some(l) = &m.link {
            plain.push('\n');
            plain.push_str(l);
        }
        let plain = truncate(&plain, limit, Unit::Chars);
        let mut h = format!("{} <b>{}</b>", m.severity.emoji(), fit(&m.title, 400, Unit::Chars, esc_html_br));
        if !m.body.is_empty() {
            h.push_str("<br/>");
            h.push_str(&fit(&m.body, limit, Unit::Chars, esc_html_br));
        }
        if let Some(l) = &m.link {
            h.push_str("<br/>");
            h.push_str(&fit(l, 1000, Unit::Chars, esc_html_br));
        }
        (plain, h)
    }

    fn event(&self, path_txn: &str, payload: serde_json::Value) -> Request {
        Request::new("PUT", format!("{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}", self.hs, pct_encode(self.room.expose()), pct_encode(path_txn)))
            .header("Authorization", format!("Bearer {}", self.token.expose()))
            .header("Content-Type", "application/json")
            .body(payload.to_string())
    }

    pub fn request(&self, m: &Message) -> Request {
        let (plain, html) = self.texts(m, LIMIT);
        let payload = serde_json::json!({"msgtype": "m.text", "body": plain, "format": "org.matrix.custom.html", "formatted_body": html});
        self.event(&format!("notts-{}-{}", m.id, self.name), payload)
    }

    /// Step 1 of a file: upload the bytes to the media repository.
    pub fn upload_request(&self, a: &crate::message::Attachment) -> Request {
        Request::new("POST", format!("{}/_matrix/media/v3/upload?filename={}", self.hs, pct_encode(&a.name)))
            .header("Authorization", format!("Bearer {}", self.token.expose()))
            .header("Content-Type", content_type_of(&a.name).split(';').next().unwrap_or("application/octet-stream").to_string())
            .body(a.data.clone())
    }

    /// Step 2: an `m.file` event that points at the uploaded content.
    pub fn file_event(&self, m: &Message, a: &crate::message::Attachment, mxc: &str) -> Request {
        let payload = serde_json::json!({
            "msgtype": "m.file",
            "body": a.name,
            "filename": a.name,
            "url": mxc,
            "info": {"mimetype": content_type_of(&a.name).split(';').next().unwrap_or("application/octet-stream"), "size": a.data.len()},
        });
        self.event(&format!("notts-{}-{}-file", m.id, self.name), payload)
    }
}

impl Channel for Matrix {
    fn kind(&self) -> &'static str {
        "matrix"
    }
    fn capacity(&self) -> Option<Capacity> {
        Some(Capacity { limit: LIMIT, max_parts: 10 })
    }
    fn cost(&self, m: &Message) -> usize {
        let (plain, html) = self.texts(m, 1 << 30);
        plain.chars().count().max(html.chars().count())
    }
    fn files(&self) -> Option<FileCaps> {
        Some(FileCaps { max_bytes: 8 * 1024 * 1024, with_text: false })
    }
    fn describe(&self, m: &Message) -> String {
        match m.attachments.first() {
            Some(a) => format!("{} then {}", self.upload_request(a).describe(), self.file_event(m, a, "mxc://...").describe()),
            None => self.request(m).describe(),
        }
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        let Some(a) = m.attachments.first() else { return http_deliver(&self.request(m), ctx) };
        let r = ctx.http.call(&self.upload_request(a))?;
        if !(200..300).contains(&r.status) {
            return crate::http::classify(r);
        }
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap_or_default();
        let Some(mxc) = v.get("content_uri").and_then(|u| u.as_str()).filter(|u| u.starts_with("mxc://")) else {
            return Err(SendError::Permanent("media upload answered without a content_uri".into()));
        };
        http_deliver(&self.file_event(m, a, mxc), ctx)
    }
    fn secrets(&self) -> Vec<String> {
        vec![self.token.expose().to_string(), self.room.expose().to_string()]
    }
}
