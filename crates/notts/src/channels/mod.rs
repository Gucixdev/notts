//! Channels: one file each. Every channel turns a [`Message`] into one delivery (an HTTP request,
//! an SMTP session, a local command) and says whether that worked.

use crate::config::ChannelSpec;
use crate::http::{Http, Request, SendError};
use crate::message::Message;
use crate::secret::{read_secret_env, read_secret_file, Secret};
use std::collections::BTreeMap;
use std::time::Duration;

pub mod desktop;
pub mod discord;
pub mod email;
pub mod matrix;
pub mod ntfy;
pub mod sms;
pub mod telegram;
pub mod webhook;

/// Environment lookup, injectable for tests.
pub type EnvFn = dyn Fn(&str) -> Option<String> + Send + Sync;

/// What a channel needs to deliver.
pub struct Ctx<'a> {
    pub http: &'a Http,
    pub timeout: Duration,
}

pub trait Channel: Send + Sync {
    fn kind(&self) -> &'static str;
    /// One line for `--dry-run`: what would be sent. Masked by the caller.
    fn describe(&self, m: &Message) -> String;
    fn deliver(&self, m: &Message, ctx: &Ctx) -> Result<(), SendError>;
    /// Private values that must never be printed.
    fn secrets(&self) -> Vec<String>;
}

pub const KINDS: &[&str] = &["telegram", "discord", "matrix", "ntfy", "webhook", "sms", "email", "desktop"];

/// Option names each kind accepts (checked at parse time, so a typo is a config error with a line).
fn allowed(kind: &str) -> &'static [&'static str] {
    match kind {
        "telegram" => &["token_file", "token_env", "chat", "chat_file", "api"],
        "discord" => &["webhook_file", "webhook_env", "username"],
        "matrix" => &["homeserver", "room", "room_file", "token_file", "token_env"],
        "ntfy" => &["server", "topic", "topic_file", "topic_env", "token_file", "token_env", "user", "pass_file", "pass_env"],
        "webhook" => &["url", "url_file", "url_env", "method", "content_type", "body", "bearer_file", "bearer_env", "user", "pass_file", "pass_env", "header"],
        "sms" => &["url", "url_file", "url_env", "method", "content_type", "body", "to", "to_file", "to_env", "max", "bearer_file", "bearer_env", "user", "pass_file", "pass_env", "header"],
        "email" => &["host", "port", "security", "user", "pass_file", "pass_env", "from", "to", "insecure_auth"],
        "desktop" => &["cmd"],
        _ => &[],
    }
}

/// Option keys whose values are private even though they are written in the config.
pub const PRIVATE_KEYS: &[&str] = &["chat", "room", "topic", "url", "to", "user", "from"];

pub fn validate(spec: &ChannelSpec) -> Result<(), String> {
    if !KINDS.contains(&spec.kind.as_str()) {
        return Err(format!("unknown channel type `{}` (known: {})", spec.kind, KINDS.join(", ")));
    }
    let ok = allowed(&spec.kind);
    for k in spec.opts.keys() {
        if !ok.contains(&k.as_str()) {
            return Err(format!("channel `{}`: unknown option `{k}` for type {} (allowed: {})", spec.name, spec.kind, ok.join(", ")));
        }
    }
    for k in ["url", "api", "server", "homeserver"] {
        if let Some(v) = spec.opts.get(k) {
            if let Some(rest) = v.split("://").nth(1) {
                if rest.split('/').next().is_some_and(|h| h.contains('@')) {
                    return Err(format!("channel `{}`: `{k}` must not contain user:password@ (use pass_file/token_file)", spec.name));
                }
            }
        }
    }
    Ok(())
}

/// Build the live channel. `Err` is the reason it is *not configured* (never a secret).
pub fn build(spec: &ChannelSpec, env: &EnvFn) -> Result<Box<dyn Channel>, String> {
    let o = Opts { spec, env };
    match spec.kind.as_str() {
        "telegram" => telegram::build(&o),
        "discord" => discord::build(&o),
        "matrix" => matrix::build(&o),
        "ntfy" => ntfy::build(&o),
        "webhook" => webhook::build(&o),
        "sms" => sms::build(&o),
        "email" => email::build(&o),
        "desktop" => desktop::build(&o),
        k => Err(format!("unknown channel type `{k}`")),
    }
}

/// Typed access to a channel's options.
pub struct Opts<'a> {
    pub spec: &'a ChannelSpec,
    pub env: &'a EnvFn,
}

impl Opts<'_> {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.spec.opts.get(k).map(|s| s.as_str())
    }
    pub fn need(&self, k: &str) -> Result<&str, String> {
        self.get(k).filter(|v| !v.is_empty()).ok_or_else(|| format!("missing option `{k}`"))
    }
    /// `<base>_file=` or `<base>_env=`: a secret that is never inline.
    pub fn secret(&self, base: &str) -> Result<Secret, String> {
        if let Some(p) = self.get(&format!("{base}_file")) {
            return read_secret_file(p);
        }
        if let Some(n) = self.get(&format!("{base}_env")) {
            return read_secret_env(n, self.env);
        }
        Err(format!("missing option `{base}_file` or `{base}_env`"))
    }
    /// A private value that may be inline (`base=`), from a file (`base_file=`) or from the environment.
    pub fn value(&self, base: &str) -> Result<Secret, String> {
        if let Some(v) = self.get(base) {
            if !v.is_empty() {
                return Ok(Secret::new(v));
            }
        }
        self.secret(base).map_err(|_| format!("missing option `{base}` (or `{base}_file`)"))
    }
    /// Optional auth: bearer token, or user + password.
    pub fn auth(&self) -> Result<Auth, String> {
        if self.get("bearer_file").is_some() || self.get("bearer_env").is_some() {
            return Ok(Auth::Bearer(self.secret("bearer")?));
        }
        if self.get("token_file").is_some() || self.get("token_env").is_some() {
            return Ok(Auth::Bearer(self.secret("token")?));
        }
        if let Some(u) = self.get("user") {
            return Ok(Auth::Basic(u.to_string(), self.secret("pass")?));
        }
        Ok(Auth::None)
    }
    pub fn url(&self, what: &str) -> Result<Secret, String> {
        let u = self.value(what)?;
        check_url(u.expose())?;
        Ok(u)
    }
    pub fn extra_header(&self) -> Result<Option<(String, String)>, String> {
        match self.get("header") {
            None => Ok(None),
            Some(h) => match h.split_once(':') {
                Some((k, v)) if !k.trim().is_empty() && !k.contains(['\r', '\n']) && !v.contains(['\r', '\n']) => Ok(Some((k.trim().to_string(), v.trim().to_string()))),
                _ => Err("option `header` must look like Name:value".into()),
            },
        }
    }
    pub fn private_values(&self) -> Vec<String> {
        let m: &BTreeMap<String, String> = &self.spec.opts;
        PRIVATE_KEYS.iter().filter_map(|k| m.get(*k).cloned()).collect()
    }
}

pub enum Auth {
    None,
    Bearer(Secret),
    Basic(String, Secret),
}

impl Auth {
    pub fn header(&self) -> Option<(String, String)> {
        match self {
            Auth::None => None,
            Auth::Bearer(t) => Some(("Authorization".into(), format!("Bearer {}", t.expose()))),
            Auth::Basic(u, p) => Some(("Authorization".into(), format!("Basic {}", crate::text::base64(format!("{u}:{}", p.expose()).as_bytes())))),
        }
    }
    pub fn secrets(&self) -> Vec<String> {
        match self {
            Auth::None => vec![],
            Auth::Bearer(t) => vec![t.expose().to_string()],
            Auth::Basic(_, p) => vec![p.expose().to_string()],
        }
    }
    pub fn apply(&self, r: Request) -> Request {
        match self.header() {
            Some((k, v)) => r.header(&k, v),
            None => r,
        }
    }
}

pub fn check_url(u: &str) -> Result<(), String> {
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err("url must start with http:// or https://".into());
    }
    if u.contains(['\r', '\n', ' ']) {
        return Err("url contains whitespace".into());
    }
    Ok(())
}

/// Shared by the channels that are one HTTP request.
pub fn http_deliver(r: &Request, ctx: &Ctx) -> Result<(), SendError> {
    ctx.http.execute(r)
}
