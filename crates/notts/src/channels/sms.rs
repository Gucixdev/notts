//! SMS through a generic HTTP gateway. There is no free official SMS API: this needs a paid
//! gateway (Twilio-style) or a self-hosted Android-gateway app on your own phone. The adapter
//! only fills a request template; it was verified against a mock server, not a real gateway.
//!
//! Placeholders: `{to}` `{text}` plus those of the webhook channel. SMS text is plain
//! "title: body" cut at `max=` characters (default 480 = three segments).

use super::*;
use crate::text::{render_template, truncate, TplMode, Unit};

pub struct Sms {
    url: Secret,
    method: String,
    content_type: String,
    template: String,
    to: Secret,
    segment: usize,
    segments: usize,
    auth: Auth,
    header: Option<(String, String)>,
}

pub fn build(o: &Opts) -> Result<Box<dyn Channel>, String> {
    let segment = match o.get("segment") {
        None => 160,
        Some(v) => v.parse::<usize>().ok().filter(|n| (40..=1600).contains(n)).ok_or("segment must be a number between 40 and 1600")?,
    };
    let segments = match o.get("segments") {
        None => 3,
        Some(v) => v.parse::<usize>().ok().filter(|n| (1..=10).contains(n)).ok_or("segments must be a number between 1 and 10")?,
    };
    Ok(Box::new(Sms {
        url: o.url("url")?,
        method: o.get("method").unwrap_or("POST").to_ascii_uppercase(),
        content_type: o.get("content_type").unwrap_or("application/json").to_string(),
        template: o.need("body")?.to_string(),
        to: o.value("to")?,
        segment,
        segments,
        auth: o.auth()?,
        header: o.extra_header()?,
    }))
}

impl Sms {
    pub fn sms_text(&self, m: &Message) -> String {
        let t = match (m.title.is_empty(), m.body.is_empty()) {
            (false, false) => format!("{}: {}", m.title, m.body.replace('\n', " ")),
            (false, true) => m.title.clone(),
            _ => m.body.replace('\n', " "),
        };
        truncate(&t, self.segment, Unit::Chars)
    }

    pub fn request(&self, m: &Message) -> Request {
        let text = self.sms_text(m);
        let full = m.text();
        let mut v = super::webhook::vars(m, &full, "");
        v.retain(|(k, _)| *k != "text");
        v.push(("text", &text));
        v.push(("to", self.to.expose()));
        let body = render_template(&self.template, &v, TplMode::for_content_type(&self.content_type));
        let mut r = Request::new(&self.method, self.url.expose()).header("Content-Type", self.content_type.clone()).body(body);
        if let Some((k, val)) = &self.header {
            r = r.header(k, val.clone());
        }
        self.auth.apply(r)
    }
}

impl Channel for Sms {
    fn kind(&self) -> &'static str {
        "sms"
    }
    fn capacity(&self) -> Option<Capacity> {
        Some(Capacity { limit: self.segment, max_parts: self.segments })
    }
    fn cost(&self, m: &Message) -> usize {
        let t = match (m.title.is_empty(), m.body.is_empty()) {
            (false, false) => format!("{}: {}", m.title, m.body.replace('\n', " ")),
            (false, true) => m.title.clone(),
            _ => m.body.replace('\n', " "),
        };
        t.chars().count()
    }
    fn describe(&self, m: &Message) -> String {
        self.request(m).describe()
    }
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError> {
        http_deliver(&self.request(m), ctx)
    }
    fn secrets(&self) -> Vec<String> {
        let mut v = vec![self.url.expose().to_string(), self.to.expose().to_string()];
        v.extend(self.auth.secrets());
        v
    }
}
