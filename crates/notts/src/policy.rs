//! The flood-control decision for one message, taken under the state lock.
//!
//! Order: flood accounting, state-change check, routing, digest / cooldown hold, grouping, quiet hours.
//! Every outcome is explainable: it carries a reason, and the engine writes it to the history.

use crate::config::Config;
use crate::message::{Message, Severity};
use crate::state::{Group, KeyState, Pending, State, MAX_PENDING};
use crate::text::fnv64;

pub struct Input<'a> {
    pub cfg: &'a Config,
    pub now: u64,
    pub local_minutes: u32,
    /// normalized message
    pub msg: &'a Message,
    /// the group / state key: explicit `--key`, or app + title
    pub key: &'a str,
    /// shown in the history and in "(x7 since ...)"
    pub key_label: &'a str,
    pub state_mode: Option<Severity>,
    pub no_group: bool,
    pub to_override: Option<&'a [String]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Deliver now (or park in the outbox when `quiet`). The message already carries any "(xN since ...)" and state text.
    Send {
        msg: Message,
        channels: Vec<String>,
        quiet: bool,
    },
    Grouped {
        channels: Vec<String>,
        count: u64,
    },
    Held {
        channels: Vec<String>,
    },
    Suppressed {
        channels: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub outcome: Outcome,
    pub reason: String,
    /// "app X is flooding": sent once when the flood guard trips
    pub notice: Option<Message>,
}

/// `HH:MM` local time of the unix time `t`, derived from the clock's local time of day.
pub fn hhmm(t: u64, now: u64, local_minutes: u32) -> String {
    let mut offset = (local_minutes as i64) * 60 + (now % 60) as i64 - (now % 86_400) as i64;
    if offset > 14 * 3600 {
        offset -= 86_400;
    }
    if offset < -12 * 3600 {
        offset += 86_400;
    }
    let local = (t as i64 + offset).rem_euclid(86_400);
    format!("{:02}:{:02}", local / 3600, local % 3600 / 60)
}

pub fn key_hash(key: &str) -> u64 {
    fnv64(&["key", key])
}

fn add_pending(st: &mut State, now: u64, app: &str, sev: Severity, title: &str, channels: &[String]) {
    let mut k = fnv64(&["pending", app, title]);
    let mut title = title.to_string();
    if !st.pending.contains_key(&k) && st.pending.len() >= MAX_PENDING {
        k = fnv64(&["pending-other", app]);
        title = "(other messages)".to_string();
    }
    let p = st.pending.entry(k).or_insert_with(|| Pending { app: app.to_string(), severity: sev, count: 0, first: now, last: now, channels: vec![], title });
    p.count += 1;
    p.last = now;
    p.first = p.first.min(now);
    p.severity = p.severity.max(sev);
    for c in channels {
        if !p.channels.contains(c) {
            p.channels.push(c.clone());
        }
    }
}

pub fn decide(st: &mut State, i: &Input) -> Decision {
    let (cfg, now) = (i.cfg, i.now);
    let app = if i.msg.app.is_empty() { "-".to_string() } else { i.msg.app.clone() };
    let mut notice = None;

    // 0. flood accounting: every incoming message counts, whatever happens to it
    let fl = st.flood.entry(app.clone()).or_default();
    fl.stamps.retain(|t| *t <= now && now - *t < 60);
    fl.stamps.push(now);
    let max = cfg.flood.max_per_minute as usize;
    if max > 0 && fl.stamps.len() > max + 1 {
        let cut = fl.stamps.len() - (max + 1);
        fl.stamps.drain(..cut);
    }
    if fl.cooldown_until > now + cfg.flood.cooldown_secs {
        fl.cooldown_until = now + cfg.flood.cooldown_secs; // the clock went back
    }
    let mut in_cooldown = fl.cooldown_until > now;
    if max > 0 && !in_cooldown && fl.stamps.len() > max {
        fl.cooldown_until = now + cfg.flood.cooldown_secs;
        fl.held = 0;
        in_cooldown = true;
        let mut n = Message::new(format!("app {app} is flooding")).app(&app).severity(Severity::Warn).tag("flood");
        n = n.body(format!(
            "{} messages from {app} in the last minute. Messages below fail are {} for {}s. Details: notts log --app {app}",
            fl.stamps.len(),
            if cfg.digest.is_some() { "collected for the digest" } else { "dropped (logged)" },
            cfg.flood.cooldown_secs
        ));
        notice = Some(n);
    }

    let mk = |outcome: Outcome, reason: String, notice: Option<Message>| Decision { outcome, reason, notice };
    let mut m = i.msg.clone();
    let mut routing = m.severity;
    let kh = key_hash(i.key);

    // 1. state-change mode
    let mut changed = false;
    if let Some(new) = i.state_mode {
        m.severity = new;
        routing = new;
        match st.states.get(&kh).cloned() {
            Some(prev) if prev.state == new => {
                return mk(Outcome::Suppressed { channels: vec![] }, format!("unchanged: state still {}", new.as_str()), notice);
            }
            Some(prev) => {
                changed = true;
                routing = prev.state.max(new);
                let since = hhmm(prev.since.min(now), now, i.local_minutes);
                let line = format!("state: {} -> {} (was {} since {since})", prev.state.as_str(), new.as_str(), prev.state.as_str());
                m.body = if m.body.is_empty() { line } else { format!("{line}\n\n{}", m.body) };
                if new == Severity::Ok {
                    m.title = format!("Resolved: {}", m.title);
                }
            }
            None => {
                if new == Severity::Ok {
                    st.states.insert(kh, KeyState { state: new, since: now, label: i.key_label.to_string() });
                    return mk(Outcome::Suppressed { channels: vec![] }, "first state is ok, nothing to resolve".into(), notice);
                }
                changed = true;
            }
        }
        st.states.insert(kh, KeyState { state: new, since: now, label: i.key_label.to_string() });
    }

    // 2. routing
    let mut channels: Vec<String> = match i.to_override {
        Some(t) => t.to_vec(),
        None => cfg.select_as(&m, routing),
    };
    // a digest with an explicit `to=` collects its severities even when no rule routes them anywhere else
    if channels.is_empty() && i.to_override.is_none() {
        if let Some(d) = &cfg.digest {
            if d.severities.contains(&routing) && !d.to.is_empty() {
                channels = d.to.clone();
            }
        }
    }
    if channels.is_empty() {
        return mk(Outcome::Suppressed { channels }, "no rule matched".into(), notice);
    }

    // 3. digest and cooldown: fail is never held here
    if routing != Severity::Fail {
        let digest_sev = cfg.digest.as_ref().is_some_and(|d| d.severities.contains(&routing));
        if cfg.digest.is_some() && (digest_sev || in_cooldown) {
            add_pending(st, now, &app, routing, &m.title, &channels);
            let reason = if digest_sev { "held for digest" } else { "held for digest (flood cooldown)" };
            if in_cooldown {
                if let Some(f) = st.flood.get_mut(&app) {
                    f.held += 1;
                }
            }
            return mk(Outcome::Held { channels }, reason.into(), notice);
        }
        if in_cooldown {
            if let Some(f) = st.flood.get_mut(&app) {
                f.held += 1;
            }
            return mk(Outcome::Suppressed { channels }, "flood cooldown, no digest configured (logged only)".into(), notice);
        }
    }

    // 4. grouping (a state change is a real event and is never grouped)
    let window = cfg.settings.group_secs;
    let mut carried = 0u64;
    let mut since = String::new();
    if !changed {
        if let Some(g) = st.groups.get_mut(&kh) {
            let live = g.last_sent <= now && now - g.last_sent < window;
            if live && !i.no_group && window > 0 {
                if g.held == 0 {
                    g.first_held = now;
                }
                g.held += 1;
                let c = g.held;
                return mk(Outcome::Grouped { channels, count: c }, format!("same key within {}s of the last send, repeat {c}", window), notice);
            }
            if g.held > 0 {
                carried = g.held;
                since = hhmm(g.first_held.min(now), now, i.local_minutes);
            }
        }
        st.groups.insert(kh, Group { label: i.key_label.to_string(), last_sent: now, held: 0, first_held: 0 });
    }
    let mut reason = "sent".to_string();
    if carried > 0 {
        m.title = format!("{} (x{} since {since})", m.title, carried + 1);
        reason = format!("sent, carries {carried} held repeat(s)");
    }

    // 5. quiet hours
    let quiet = cfg.quiet_holds(routing, i.local_minutes);
    if quiet {
        reason = "held by quiet hours".into();
    }
    mk(Outcome::Send { msg: m, channels, quiet }, reason, notice)
}

/// Put a digest row back (a digest nobody took).
pub fn merge_pending(st: &mut State, r: &Pending) {
    let k = fnv64(&["pending", &r.app, &r.title]);
    match st.pending.get_mut(&k) {
        Some(p) => {
            p.count += r.count;
            p.first = p.first.min(r.first);
            p.last = p.last.max(r.last);
            p.severity = p.severity.max(r.severity);
            for c in &r.channels {
                if !p.channels.contains(c) {
                    p.channels.push(c.clone());
                }
            }
        }
        None => {
            st.pending.insert(k, r.clone());
        }
    }
}
