//! ntfy (https://ntfy.sh or self-hosted): free push to a phone app. Published as JSON to the
//! server root, so titles with Polish letters or emoji need no header encoding.
//! The topic is the only access control on a public server, so it is treated as a secret.
//! Limit: 4096 bytes (server default); cut at 4000 bytes.

use super::*;
use crate::text::{truncate, Unit};

pub const LIMIT_BYTES: usize = 4000;
const DEFAULT_SERVER: &str = "https://ntfy.sh";

pub struct Ntfy {
    server: String,
    topic: Secret,
    auth: Auth,
}

pub fn build(o: &Opts) -> Result<Box<dyn Channel>, String> {
    let server = o.get("server").unwrap_or(DEFAULT_SERVER).trim_end_matches('/').to_string();
    check_url(&server)?;
    let topic = o.value("topic")?;
    if topic.expose().contains(['/', ' ', '\n']) {
        return Err("topic must not contain / or spaces".into());
    }
    Ok(Box::new(Ntfy { server, topic, auth: o.auth()? }))
}

impl Ntfy {
    /// A file is a raw `PUT` to the topic; text and title go in headers (RFC 2047 when not ASCII).
    pub fn file_request(&self, m: &Message, a: &crate::message::Attachment) -> Request {
        let hv = |s: &str| if s.is_ascii() && !s.contains(['\r', '\n']) { s.to_string() } else { format!("=?UTF-8?B?{}?=", crate::text::base64(s.replace(['\r', '\n'], " ").as_bytes())) };
        let (prio, tag) = match m.severity {
            crate::message::Severity::Fail => (5, "rotating_light"),
            crate::message::Severity::Warn => (4, "warning"),
            crate::message::Severity::Ok => (3, "white_check_mark"),
            crate::message::Severity::Info => (3, "information_source"),
        };
        let msg = truncate(&if m.body.is_empty() { m.title.clone() } else { m.body.clone() }, LIMIT_BYTES, Unit::Bytes);
        let mut r = Request::new("PUT", format!("{}/{}", self.server, self.topic.expose()))
            .header("Filename", hv(&a.name))
            .header("Message", hv(&msg))
            .header("Priority", prio.to_string())
            .header("Tags", tag)
            .header("Content-Type", content_type_of(&a.name).split(';').next().unwrap_or("application/octet-stream").to_string())
            .body(a.data.clone());
        if !m.title.is_empty() && !m.body.is_empty() {
            r = r.header("Title", hv(&truncate(&m.title, 250, Unit::Bytes)));
        }
        if let Some(l) = &m.link {
            r = r.header("Click", hv(l));
        }
        self.auth.apply(r)
    }

    pub fn request(&self, m: &Message) -> Request {
        use crate::message::Severity::*;
        let (prio, tag) = match m.severity {
            Info => (3, "information_source"),
            Ok => (3, "white_check_mark"),
            Warn => (4, "warning"),
            Fail => (5, "rotating_light"),
        };
        let mut p = serde_json::json!({
            "topic": self.topic.expose(),
            "message": truncate(&if m.body.is_empty() { m.title.clone() } else { m.body.clone() }, LIMIT_BYTES, Unit::Bytes),
            "priority": prio,
            "tags": [tag],
        });
        if !m.title.is_empty() && !m.body.is_empty() {
            p["title"] = serde_json::Value::String(truncate(&m.title, 250, Unit::Bytes));
        }
        if let Some(l) = &m.link {
            p["click"] = serde_json::Value::String(l.clone());
        }
        self.auth.apply(Request::new("POST", format!("{}/", self.server)).header("Content-Type", "application/json").body(p.to_string()))
    }
}

impl Channel for Ntfy {
    fn kind(&self) -> &'static str {
        "ntfy"
    }
    fn capacity(&self) -> Option<Capacity> {
        Some(Capacity { limit: LIMIT_BYTES, max_parts: 10 })
    }
    fn cost(&self, m: &Message) -> usize {
        if m.body.is_empty() { m.title.len() } else { m.body.len() }
    }
    fn files(&self) -> Option<FileCaps> {
        Some(FileCaps { max_bytes: 8 * 1024 * 1024, with_text: true })
    }
    fn describe(&self, m: &Message) -> String {
        match m.attachments.first() {
            Some(a) => self.file_request(m, a).describe(),
            None => self.request(m).describe(),
        }
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        match m.attachments.first() {
            Some(a) => http_deliver(&self.file_request(m, a), ctx),
            None => http_deliver(&self.request(m), ctx),
        }
    }
    fn secrets(&self) -> Vec<String> {
        let mut v = vec![self.topic.expose().to_string()];
        v.extend(self.auth.secrets());
        v
    }
}
