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

pub fn vars<'a>(m: &'a Message, text: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![("app", &m.app), ("title", &m.title), ("body", &m.body), ("text", text), ("severity", m.severity.as_str()), ("link", m.link.as_deref().unwrap_or("")), ("id", &m.id)]
}

impl Webhook {
    pub fn request(&self, m: &Message) -> Request {
        let text = m.text();
        let body = match &self.template {
            Some(t) => render_template(t, &vars(m, &text), TplMode::for_content_type(&self.content_type)),
            None => serde_json::json!({"app": m.app, "title": m.title, "body": m.body, "text": text, "severity": m.severity.as_str(), "link": m.link, "id": m.id}).to_string(),
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
