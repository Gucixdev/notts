//! The engine: policy decision (grouping, state changes, digest, flood guard), routing, delivery
//! with retries, outbox and history. This is what the library API and the CLI both drive.

use crate::channels::{self, Channel, Ctx, EnvFn};
use crate::clock::{Clock, SystemClock};
use crate::config::{Config, ConfigError};
use crate::history::{Entry as HEntry, History};
use crate::http::{Http, SendError};
use crate::message::{Message, Severity};
use crate::outbox::{Entry as OEntry, Outbox};
use crate::policy::{self, Input, Outcome};
use crate::secret::Masker;
use crate::state::{Pending, Store};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Version of the CLI/JSON contract (README, "Contract"). Bumped only for incompatible changes.
pub const CONTRACT: u32 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Sent,
    Failed,
    Skipped,
    NotConfigured,
    Deduped,
    Queued,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Sent => "sent",
            Status::Failed => "failed",
            Status::Skipped => "skipped",
            Status::NotConfigured => "not-configured",
            Status::Deduped => "deduped",
            Status::Queued => "queued",
        }
    }
}

/// What happened to the message as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Sent,
    Grouped,
    Held,
    Suppressed,
    Queued,
    Failed,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Sent => "sent",
            Disposition::Grouped => "grouped",
            Disposition::Held => "held",
            Disposition::Suppressed => "suppressed",
            Disposition::Queued => "queued",
            Disposition::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelResult {
    pub name: String,
    pub status: Status,
    /// human text, never a secret
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub results: Vec<ChannelResult>,
    pub exit_code: i32,
    pub disposition: Disposition,
    pub reason: String,
    pub key: String,
}

fn jstr(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

impl Report {
    /// The frozen JSON shape (key order is fixed, see the golden test).
    pub fn to_json(&self) -> String {
        let rs: Vec<String> = self.results.iter().map(|r| format!("{{\"name\":{},\"status\":{},\"detail\":{}}}", jstr(&r.name), jstr(r.status.as_str()), jstr(&r.detail))).collect();
        format!(
            "{{\"notts_version\":{},\"contract\":{},\"exit_code\":{},\"disposition\":{},\"reason\":{},\"key\":{},\"results\":[{}]}}",
            jstr(VERSION),
            CONTRACT,
            self.exit_code,
            jstr(self.disposition.as_str()),
            jstr(&self.reason),
            jstr(&self.key),
            rs.join(",")
        )
    }
}

#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct SendOptions {
    pub dry_run: bool,
    /// do not group repeats (state-change mode is unaffected)
    pub no_group: bool,
    /// send to exactly these channels, ignoring rules
    pub to: Option<Vec<String>>,
    /// group/state key; default is app + title
    pub key: Option<String>,
    /// state-change mode: notify only when this key's state changes
    pub state: Option<Severity>,
}

impl SendOptions {
    pub fn new() -> SendOptions {
        SendOptions::default()
    }
    pub fn dry_run(mut self, v: bool) -> Self {
        self.dry_run = v;
        self
    }
    pub fn no_group(mut self, v: bool) -> Self {
        self.no_group = v;
        self
    }
    pub fn to(mut self, v: Vec<String>) -> Self {
        self.to = Some(v);
        self
    }
    pub fn key(mut self, v: impl Into<String>) -> Self {
        self.key = Some(v.into());
        self
    }
    pub fn state(mut self, v: Severity) -> Self {
        self.state = Some(v);
        self
    }
}

#[derive(Debug)]
pub enum Error {
    Config(ConfigError),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(e) => e.fmt(f),
            Error::Io(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

/// Everything that can be injected: tests use a fake clock, a private state dir and a fake environment.
#[derive(Clone)]
pub struct Init {
    pub clock: Arc<dyn Clock>,
    pub state_dir: Option<PathBuf>,
    pub env: Arc<EnvFn>,
    pub env_proxy: bool,
}

impl Default for Init {
    fn default() -> Self {
        Init { clock: Arc::new(SystemClock), state_dir: None, env: Arc::new(|k| std::env::var(k).ok()), env_proxy: true }
    }
}

struct Built {
    name: String,
    required: bool,
    chan: Result<Box<dyn Channel>, String>,
}

pub struct Notts {
    cfg: Config,
    built: Vec<Built>,
    masker: Masker,
    clock: Arc<dyn Clock>,
    http: Http,
    store: Store,
    outbox: Outbox,
    history: History,
    state_dir: PathBuf,
}

pub fn default_state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state"))).unwrap_or_else(|| PathBuf::from("."));
    base.join("notts")
}

pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))).unwrap_or_else(|| PathBuf::from("."));
    base.join("notts/notts.conf")
}

impl Notts {
    /// Load the config file and resolve every channel's secrets.
    pub fn from_config(path: &Path) -> Result<Notts, Error> {
        Notts::from_config_with(path, Init::default())
    }

    pub fn from_config_with(path: &Path, init: Init) -> Result<Notts, Error> {
        let text = std::fs::read(path).map_err(|e| Error::Io(format!("cannot read config {} ({:?})", path.display(), e.kind())))?;
        Notts::from_str_with(&String::from_utf8_lossy(&text), init)
    }

    pub fn from_config_str(text: &str) -> Result<Notts, Error> {
        Notts::from_str_with(text, Init::default())
    }

    pub fn from_str_with(text: &str, init: Init) -> Result<Notts, Error> {
        let cfg = Config::parse(text).map_err(Error::Config)?;
        Ok(Notts::new(cfg, init))
    }

    pub fn new(cfg: Config, init: Init) -> Notts {
        let mut masker = Masker::new();
        let mut built = Vec::new();
        for spec in &cfg.channels {
            let o = channels::Opts { spec, env: &*init.env };
            for v in o.private_values() {
                masker.add(&v);
            }
            let chan = channels::build(spec, &*init.env);
            if let Ok(c) = &chan {
                for s in c.secrets() {
                    masker.add(&s);
                }
            }
            built.push(Built { name: spec.name.clone(), required: spec.required, chan });
        }
        let state_dir = init.state_dir.clone().or_else(|| cfg.settings.state_dir.as_deref().map(crate::secret::expand_tilde)).unwrap_or_else(default_state_dir);
        Notts {
            http: Http::new(Duration::from_secs(cfg.settings.timeout_secs), init.env_proxy),
            store: Store::new(&state_dir),
            outbox: Outbox::new(&state_dir),
            history: History::new(&state_dir),
            state_dir,
            cfg,
            built,
            masker,
            clock: init.clock,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
    pub fn mask(&self, s: &str) -> String {
        self.masker.mask(s)
    }
    pub fn outbox_len(&self) -> usize {
        self.outbox.count()
    }
    pub fn history(&self) -> Vec<HEntry> {
        self.history.read()
    }

    /// `(name, kind, Ok | reason it is not configured)`; never a secret.
    pub fn channel_status(&self) -> Vec<(String, String, Result<(), String>)> {
        self.built
            .iter()
            .zip(&self.cfg.channels)
            .map(|(b, s)| (b.name.clone(), s.kind.clone(), b.chan.as_ref().map(|_| ()).map_err(|e| self.masker.mask(e))))
            .collect()
    }

    fn log(&self, now: u64, decision: &str, m: &Message, key: &str, reason: &str) {
        self.history.append(&HEntry { at: now, decision: decision.into(), app: m.app.clone(), severity: m.severity.as_str().into(), key: key.into(), reason: self.masker.mask(reason), title: m.title.clone() });
    }

    // ---- sending -------------------------------------------------------------------------

    /// Take one message through flood control, routing and delivery. Never panics.
    pub fn send(&self, m: &Message, o: &SendOptions) -> Report {
        let msg = m.normalized();
        let now = self.clock.unix();
        let lm = self.clock.local_minutes();
        if !o.dry_run {
            let _ = self.digest(false, false);
        }
        let key = o.key.clone().unwrap_or_else(|| format!("{}\n{}", msg.app, msg.title));
        let label = o.key.clone().unwrap_or_else(|| msg.title.clone());
        let input = Input { cfg: &self.cfg, now, local_minutes: lm, msg: &msg, key: &key, key_label: &label, state_mode: o.state, no_group: o.no_group, to_override: o.to.as_deref() };
        let window = self.cfg.settings.group_secs;
        let decided = self.store.transact(!o.dry_run, |st| {
            if !o.dry_run {
                st.prune(now, window);
            }
            policy::decide(st, &input)
        });
        let (dec, failed_open) = match decided {
            Ok(d) => (d, None),
            Err(e) => {
                // never lose a message because the state is unusable: send it plainly
                let channels = o.to.clone().unwrap_or_else(|| self.cfg.select(&msg));
                let quiet = self.cfg.quiet_holds(msg.severity, lm);
                (policy::Decision { outcome: Outcome::Send { msg: msg.clone(), channels, quiet }, reason: "sent".into(), notice: None }, Some(format!("flood-control state unusable ({:?}); sent without grouping", e.kind())))
            }
        };
        let mut reason = dec.reason.clone();
        if let Some(f) = failed_open {
            reason = f;
        }
        let (mut results, disposition, selected): (Vec<ChannelResult>, Disposition, Vec<String>) = match dec.outcome {
            Outcome::Grouped { channels, count } => {
                let r = channels.iter().map(|c| ChannelResult { name: c.clone(), status: Status::Deduped, detail: format!("grouped: repeat {count} counted, not sent") }).collect();
                (r, Disposition::Grouped, channels)
            }
            Outcome::Held { channels } => {
                let r = channels.iter().map(|c| ChannelResult { name: c.clone(), status: Status::Skipped, detail: reason.clone() }).collect();
                (r, Disposition::Held, channels)
            }
            Outcome::Suppressed { channels } => {
                let r = channels.iter().map(|c| ChannelResult { name: c.clone(), status: Status::Skipped, detail: reason.clone() }).collect();
                (r, Disposition::Suppressed, channels)
            }
            Outcome::Send { msg: out, channels, quiet } => {
                let r = self.deliver_to(&channels, &out, if quiet { Some("held: quiet hours") } else { None }, o.dry_run);
                let d = disposition_of(&r, o.dry_run);
                if d == Disposition::Suppressed {
                    reason = "dry-run".into();
                } else {
                    let summary: Vec<String> = r.iter().map(|c| format!("{}={}", c.name, c.status.as_str())).collect();
                    reason = format!("{reason}; {}", summary.join(" "));
                }
                (r, d, channels)
            }
        };
        // every configured channel appears, so the caller sees why one did not take part
        let mut full = Vec::new();
        for b in &self.built {
            match results.iter().position(|r| r.name == b.name) {
                Some(i) => full.push(results.remove(i)),
                None => full.push(ChannelResult { name: b.name.clone(), status: Status::Skipped, detail: if selected.is_empty() { reason.clone() } else { "not selected by rules".into() } }),
            }
        }
        full.append(&mut results); // unknown names from --to
        if !o.dry_run {
            self.log(now, disposition.as_str(), &msg, &label, &reason);
        }
        if let (Some(n), false) = (dec.notice, o.dry_run) {
            self.send_notice(&n, now);
        }
        let exit_code = exit_code(&full, &selected, &self.built);
        let mut report = Report { results: full, exit_code, disposition, reason, key: label };
        self.mask_report(&mut report);
        report
    }

    fn mask_report(&self, r: &mut Report) {
        r.reason = self.masker.mask(&r.reason);
        r.key = self.masker.mask(&r.key);
        for c in &mut r.results {
            c.detail = self.masker.mask(&c.detail);
        }
    }

    fn send_notice(&self, n: &Message, now: u64) {
        let n = n.normalized();
        let channels = self.cfg.select(&n);
        let quiet = self.cfg.quiet_holds(n.severity, self.clock.local_minutes());
        let r = self.deliver_to(&channels, &n, if quiet { Some("held: quiet hours") } else { None }, false);
        let summary: Vec<String> = r.iter().map(|c| format!("{}={}", c.name, c.status.as_str())).collect();
        self.log(now, "notice", &n, &n.app, &summary.join(" "));
    }

    /// Deliver to the named channels, one thread each; a failure in one never blocks the others.
    /// `hold` = park in the outbox instead of sending (quiet hours).
    fn deliver_to(&self, names: &[String], msg: &Message, hold: Option<&str>, dry: bool) -> Vec<ChannelResult> {
        let mut out: Vec<Option<ChannelResult>> = vec![None; names.len()];
        let mut work: Vec<(usize, &str, &dyn Channel)> = Vec::new();
        for (i, n) in names.iter().enumerate() {
            match self.built.iter().find(|b| &b.name == n) {
                None => out[i] = Some(ChannelResult { name: n.clone(), status: Status::NotConfigured, detail: "no such channel".into() }),
                Some(b) => match &b.chan {
                    Err(why) => out[i] = Some(ChannelResult { name: n.clone(), status: Status::NotConfigured, detail: why.clone() }),
                    Ok(c) if dry => out[i] = Some(ChannelResult { name: n.clone(), status: Status::Skipped, detail: format!("dry-run: would send via {}", self.masker.mask(&c.describe(msg))) }),
                    Ok(c) => work.push((i, n, c.as_ref())),
                },
            }
        }
        let mut to_queue: Vec<OEntry> = Vec::new();
        let now = self.clock.unix();
        let queue = |name: &str, why: &str| OEntry { msg: msg.clone(), channel: name.to_string(), created: now, reason: why.to_string() };
        if let Some(why) = hold {
            for (i, n, _) in &work {
                to_queue.push(queue(n, why));
                out[*i] = Some(ChannelResult { name: n.to_string(), status: Status::Queued, detail: why.into() });
            }
        } else if !work.is_empty() {
            let outcomes: Vec<(usize, Result<u32, (SendError, u32)>)> = std::thread::scope(|s| {
                let handles: Vec<_> = work.iter().map(|(i, _, c)| (*i, s.spawn(move || self.attempt(*c, msg)))).collect();
                handles.into_iter().map(|(i, h)| (i, h.join().unwrap_or_else(|_| Err((SendError::Permanent("internal error in channel".into()), 0))))).collect()
            });
            for (i, res) in outcomes {
                let name = names[i].clone();
                out[i] = Some(match res {
                    Ok(n) => ChannelResult { name, status: Status::Sent, detail: format!("delivered on attempt {n}") },
                    Err((SendError::Permanent(d), _)) => ChannelResult { name, status: Status::Failed, detail: format!("{d} (permanent, not retried)") },
                    Err((SendError::Retryable { detail, .. }, n)) => {
                        to_queue.push(queue(&name, &detail));
                        ChannelResult { name, status: Status::Queued, detail: format!("{detail} after {n} attempt(s); queued in outbox") }
                    }
                });
            }
        }
        if !to_queue.is_empty() {
            if let Err(e) = self.outbox.append(&to_queue) {
                for q in &to_queue {
                    if let Some(Some(r)) = out.iter_mut().find(|r| r.as_ref().is_some_and(|r| r.name == q.channel)) {
                        r.status = Status::Failed;
                        r.detail = format!("{}; outbox write failed ({:?})", r.detail, e.kind());
                    }
                }
            }
        }
        out.into_iter().flatten().collect()
    }

    /// One channel with bounded retries. Backoff doubles from `backoff`, a server's Retry-After wins
    /// when it is at most a minute, otherwise we stop and let the outbox retry later.
    fn attempt(&self, c: &dyn Channel, msg: &Message) -> Result<u32, (SendError, u32)> {
        let ctx = Ctx { http: &self.http, timeout: Duration::from_secs(self.cfg.settings.timeout_secs) };
        let max = 1 + self.cfg.settings.retries;
        let base = Duration::from_millis(self.cfg.settings.backoff_ms);
        let mut n = 0;
        loop {
            n += 1;
            match c.deliver(msg, &ctx) {
                Ok(()) => return Ok(n),
                Err(e @ SendError::Permanent(_)) => return Err((e, n)),
                Err(SendError::Retryable { detail, after }) => {
                    let give_up = n >= max || after.is_some_and(|a| a > MAX_RETRY_AFTER);
                    if give_up {
                        return Err((SendError::Retryable { detail, after }, n));
                    }
                    let wait = after.unwrap_or_else(|| (base * 2u32.saturating_pow(n - 1)).min(MAX_BACKOFF));
                    self.clock.sleep(wait);
                }
            }
        }
    }

    // ---- digest --------------------------------------------------------------------------

    /// Send the digest when it is due (or `force`). `dry` only composes it. `None` when nothing was to be sent.
    pub fn digest(&self, force: bool, dry: bool) -> Option<Report> {
        let d = self.cfg.digest.as_ref()?;
        let (now, lm) = (self.clock.unix(), self.clock.local_minutes());
        let rows: Vec<Pending> = if dry {
            self.store.transact(false, |st| st.pending.values().cloned().collect()).ok()?
        } else {
            self.store
                .transact(true, |st| {
                    if !(force || crate::digest::due(&self.cfg, st, now, lm)) {
                        return Vec::new();
                    }
                    st.last_digest = Some(now);
                    std::mem::take(&mut st.pending).into_values().collect()
                })
                .ok()?
        };
        let (msg, channels) = crate::digest::compose(d, &rows, now, lm)?;
        let quiet = self.cfg.quiet_holds(msg.severity, lm);
        let msg = msg.normalized();
        let results = self.deliver_to(&channels, &msg, if quiet && !dry { Some("held: quiet hours") } else { None }, dry);
        let took = results.iter().any(|r| matches!(r.status, Status::Sent | Status::Queued));
        if !dry {
            if !took {
                // nobody took it: keep the rows for the next period
                let _ = self.store.transact(true, |st| {
                    for r in &rows {
                        policy::merge_pending(st, r);
                    }
                });
            }
            let summary: Vec<String> = results.iter().map(|c| format!("{}={}", c.name, c.status.as_str())).collect();
            self.log(now, if took { "digest" } else { "digest-failed" }, &msg, "digest", &summary.join(" "));
        }
        let disposition = if dry { Disposition::Suppressed } else { disposition_of(&results, false) };
        let exit_code = exit_code(&results, &channels, &self.built);
        let mut r = Report { results, exit_code, disposition, reason: if dry { "dry-run".into() } else { "digest".into() }, key: "digest".into() };
        self.mask_report(&mut r);
        Some(r)
    }

    // ---- outbox flush --------------------------------------------------------------------

    /// Retry everything in the outbox. Entries older than `max_age_hours` and entries a server
    /// refused for good go to `outbox.dead`.
    pub fn flush(&self) -> FlushReport {
        let (entries, torn) = self.outbox.load();
        let now = self.clock.unix();
        let lm = self.clock.local_minutes();
        let max_age = self.cfg.settings.max_age_hours * 3600;
        let (mut done, mut dead, mut delivered, mut kept) = (Vec::new(), Vec::<OEntry>::new(), 0usize, 0usize);
        let mut down: Vec<String> = Vec::new();
        let mut notes = Vec::new();
        for e in entries {
            if max_age > 0 && now.saturating_sub(e.created) > max_age {
                notes.push(format!("{}: expired after {}h, moved to outbox.dead", e.channel, self.cfg.settings.max_age_hours));
                done.push(e.key());
                dead.push(e);
                continue;
            }
            if down.contains(&e.channel) || self.cfg.quiet_holds(e.msg.severity, lm) {
                kept += 1;
                continue;
            }
            let Some(b) = self.built.iter().find(|b| b.name == e.channel) else {
                kept += 1;
                continue;
            };
            let Ok(c) = &b.chan else {
                kept += 1;
                continue;
            };
            match self.attempt(c.as_ref(), &e.msg) {
                Ok(_) => {
                    delivered += 1;
                    done.push(e.key());
                    self.log(now, "flushed", &e.msg, &e.msg.title, &format!("{} delivered from outbox", e.channel));
                }
                Err((SendError::Permanent(d), _)) => {
                    notes.push(format!("{}: {} (permanent), moved to outbox.dead", e.channel, self.masker.mask(&d)));
                    done.push(e.key());
                    dead.push(e);
                }
                Err((SendError::Retryable { detail, .. }, _)) => {
                    notes.push(format!("{}: {} (will retry)", e.channel, self.masker.mask(&detail)));
                    down.push(e.channel.clone());
                    kept += 1;
                }
            }
        }
        let _ = self.outbox.bury(&dead);
        let _ = self.outbox.remove(&done);
        FlushReport { delivered, remaining: kept, buried: dead.len(), torn_lines: torn, notes }
    }
}

#[derive(Clone, Debug)]
pub struct FlushReport {
    pub delivered: usize,
    pub remaining: usize,
    pub buried: usize,
    pub torn_lines: usize,
    pub notes: Vec<String>,
}

impl FlushReport {
    pub fn exit_code(&self) -> i32 {
        if self.remaining == 0 {
            0
        } else if self.delivered > 0 {
            0
        } else {
            3
        }
    }
}

fn disposition_of(r: &[ChannelResult], dry: bool) -> Disposition {
    if dry {
        return Disposition::Suppressed;
    }
    let any = |s: Status| r.iter().any(|c| c.status == s);
    if any(Status::Sent) {
        Disposition::Sent
    } else if any(Status::Queued) {
        Disposition::Queued
    } else if any(Status::Failed) || any(Status::NotConfigured) {
        Disposition::Failed
    } else {
        Disposition::Suppressed
    }
}

/// 0 = delivered to at least one required channel (or deliberately not sent: grouped, held,
/// suppressed, dry run); 3 = nothing delivered but something is queued in the outbox; 1 = failed.
/// "Required" are the selected channels marked `required=yes`, or all selected ones if none is marked.
fn exit_code(results: &[ChannelResult], selected: &[String], built: &[Built]) -> i32 {
    let marked: Vec<&str> = built.iter().filter(|b| b.required && selected.contains(&b.name)).map(|b| b.name.as_str()).collect();
    let is_required = |n: &str| if marked.is_empty() { selected.iter().any(|s| s == n) } else { marked.contains(&n) };
    if results.iter().any(|r| r.status == Status::Sent && is_required(&r.name)) {
        return 0;
    }
    if results.iter().any(|r| r.status == Status::Queued) {
        return 3;
    }
    if results.iter().any(|r| matches!(r.status, Status::Failed | Status::NotConfigured) && selected.contains(&r.name)) {
        return 1;
    }
    if results.iter().any(|r| r.status == Status::Sent) {
        // delivered, but not to a required channel
        return 1;
    }
    0
}
