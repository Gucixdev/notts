//! Generic webhook: any URL, method, content type and body template.
//! Placeholders: `{app} {title} {body} {text} {severity} {link} {id}`; values are escaped for the
//! content type (JSON string escaping, form encoding, or raw). Default body is a JSON object.

use super::*;
use crate::text::{render_template, TplMode};

pub struct Webhook {
    url: Secret,
    method: String,
    content_type: String,
    template: Option<String>,
    auth: Auth,
    header: Option<(String, String)>,
}

pub fn build(o: &Opts) -> Result<Box<dyn Channel>, String> {
    let method = o.get("method").unwrap_or("POST").to_ascii_uppercase();
    if !matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "GET") {
        return Err("method must be POST, PUT, PATCH or GET".into());
    }
    Ok(Box::new(Webhook {
        url: o.url("url")?,
        method,
        content_type: o.get("content_type").unwrap_or("application/json").to_string(),
        template: o.get("body").map(String::from),
        auth: o.auth()?,
        header: o.extra_header()?,
    }))
}

pub fn vars<'a>(m: &'a Message, text: &'a str, attachment: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![("attachment", attachment), ("app", &m.app), ("title", &m.title), ("body", &m.body), ("text", text), ("severity", m.severity.as_str()), ("link", m.link.as_deref().unwrap_or("")), ("id", &m.id)]
}

impl Webhook {
    pub fn request(&self, m: &Message) -> Request {
        let text = m.text();
        let att_text = m.attachments.first().map(|a| String::from_utf8_lossy(&a.data).into_owned()).unwrap_or_default();
        let body = match &self.template {
            Some(t) => render_template(t, &vars(m, &text, &att_text), TplMode::for_content_type(&self.content_type)),
            None => {
                let mut j = serde_json::json!({"app": m.app, "title": m.title, "body": m.body, "text": text, "severity": m.severity.as_str(), "link": m.link, "id": m.id});
                if !m.attachments.is_empty() {
                    // the report travels inline: text when it is UTF-8, base64 otherwise
                    let list: Vec<serde_json::Value> = m
                        .attachments
                        .iter()
                        .map(|a| match a.text() {
                            Some(t) => serde_json::json!({"name": a.name, "size": a.data.len(), "sha256": a.sha256(), "text": t}),
                            None => serde_json::json!({"name": a.name, "size": a.data.len(), "sha256": a.sha256(), "base64": crate::text::base64(&a.data)}),
                        })
                        .collect();
                    j["attachments"] = serde_json::Value::Array(list);
                }
                j.to_string()
            }
        };
        let mut r = Request::new(&self.method, self.url.expose()).header("Content-Type", self.content_type.clone()).body(body);
        if let Some((k, v)) = &self.header {
            r = r.header(k, v.clone());
        }
        self.auth.apply(r)
    }
}

impl Channel for Webhook {
    fn kind(&self) -> &'static str {
        "webhook"
    }
    fn files(&self) -> Option<FileCaps> {
        match &self.template {
            None => Some(FileCaps { max_bytes: 512 * 1024, with_text: true }),
            Some(t) if t.contains("{attachment}") => Some(FileCaps { max_bytes: 512 * 1024, with_text: true }),
            Some(_) => None,
        }
    }
    fn describe(&self, m: &Message) -> String {
        self.request(m).describe()
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        http_deliver(&self.request(m), ctx)
    }
    fn secrets(&self) -> Vec<String> {
        let mut v = vec![self.url.expose().to_string()];
        v.extend(self.auth.secrets());
        v
    }
}
