//! The config file: one readable text file, `directive args key=value` per line.
//!
//! ```text
//! # comment
//! set timeout=10 retries=2 group=10m
//! channel telegram token_file=~/.secrets/notts-telegram chat=123
//! channel phone type=ntfy topic_file=~/.secrets/notts-topic
//! rule app=judged min=warn to=telegram,phone
//! quiet 22:00-07:00 except=fail
//! ```
//! Secrets are never in this file, only paths of mode-600 files or environment variable names.

use crate::message::{Message, Severity};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "config: {}", self.msg)
        } else {
            write!(f, "config line {}: {}", self.line, self.msg)
        }
    }
}

impl std::error::Error for ConfigError {}

fn err<T>(line: usize, msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError { line, msg: msg.into() })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub timeout_secs: u64,
    pub retries: u32,
    pub backoff_ms: u64,
    /// Repeats of one key within this window are counted, not sent.
    pub group_secs: u64,
    pub max_age_hours: u64,
    pub state_dir: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { timeout_secs: 10, retries: 2, backoff_ms: 1000, group_secs: 600, max_age_hours: 168, state_dir: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSpec {
    pub name: String,
    pub kind: String,
    pub opts: BTreeMap<String, String>,
    pub required: bool,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub app: Option<String>,
    pub tag: Option<String>,
    pub min: Severity,
    pub to: Vec<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quiet {
    /// minutes since midnight, inclusive
    pub start: u32,
    /// minutes since midnight, exclusive; 1440 = 24:00
    pub end: u32,
    pub except: Vec<Severity>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// every N seconds after the first held message
    Every(u64),
    /// at fixed local times (minutes since midnight, ascending)
    At(Vec<u32>),
}

/// `digest every=30m` or `digest at=08:00,18:00`, plus `severities=info,warn lines=30 to=a,b`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestCfg {
    pub schedule: Schedule,
    pub severities: Vec<Severity>,
    pub lines: usize,
    /// empty = the channels the held messages were routed to
    pub to: Vec<String>,
    pub line: usize,
}

/// `flood max=30 cooldown=10m` (messages per minute per app); `flood off` disables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloodCfg {
    pub max_per_minute: u32,
    pub cooldown_secs: u64,
}

impl Default for FloodCfg {
    fn default() -> Self {
        FloodCfg { max_per_minute: 30, cooldown_secs: 600 }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub settings: Settings,
    pub channels: Vec<ChannelSpec>,
    pub rules: Vec<Rule>,
    pub quiet: Vec<Quiet>,
    pub digest: Option<DigestCfg>,
    pub flood: FloodCfg,
}

/// `90s`, `30m`, `2h`, `1d`. A bare number is refused: the unit must be written.
pub fn parse_duration(v: &str) -> Result<u64, String> {
    let (n, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: u64 = n.parse().map_err(|_| format!("`{v}` is not a duration (use 30s, 10m, 2h, 1d)"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "" => return Err(format!("`{v}` needs a unit (30s, 10m, 2h, 1d)")),
        _ => return Err(format!("`{v}` has an unknown unit (use s, m, h, d)")),
    };
    n.checked_mul(mult).ok_or_else(|| format!("`{v}` is too large"))
}

/// Split a line into tokens: whitespace separated, `"double quotes"` group (with `\"` `\\` `\n`),
/// `#` at the start of a token begins a comment.
pub fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut in_q = false;
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if in_q {
            match c {
                '"' => in_q = false,
                '\\' => match it.next() {
                    Some('n') => cur.push('\n'),
                    Some(o) => cur.push(o),
                    None => return Err("dangling backslash".into()),
                },
                _ => cur.push(c),
            }
        } else if c == '"' {
            in_q = true;
            have = true;
        } else if c.is_whitespace() {
            if have {
                toks.push(std::mem::take(&mut cur));
                have = false;
            }
        } else if c == '#' && !have {
            break;
        } else {
            cur.push(c);
            have = true;
        }
    }
    if in_q {
        return Err("unterminated quote".into());
    }
    if have {
        toks.push(cur);
    }
    Ok(toks)
}

/// `HH:MM`, 24:00 allowed (end of day).
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 || !h.bytes().all(|b| b.is_ascii_digit()) || !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    if m > 59 || h > 24 || (h == 24 && m != 0) {
        return None;
    }
    Some(h * 60 + m)
}

fn split_kv(tok: &str) -> Option<(&str, &str)> {
    let (k, v) = tok.split_once('=')?;
    if k.is_empty() {
        return None;
    }
    Some((k, v))
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut cfg = Config::default();
        for (i, raw) in text.lines().enumerate() {
            let ln = i + 1;
            let toks = match tokenize(raw) {
                Ok(t) => t,
                Err(e) => return err(ln, e),
            };
            let Some((dir, args)) = toks.split_first() else { continue };
            match dir.as_str() {
                "set" => cfg.parse_set(ln, args)?,
                "channel" => cfg.parse_channel(ln, args)?,
                "rule" => cfg.parse_rule(ln, args)?,
                "quiet" => cfg.parse_quiet(ln, args)?,
                "digest" => cfg.parse_digest(ln, args)?,
                "flood" => cfg.parse_flood(ln, args)?,
                other => return err(ln, format!("unknown directive `{other}` (expected set, channel, rule or quiet)")),
            }
        }
        for r in &cfg.rules {
            for t in &r.to {
                if !cfg.channels.iter().any(|c| &c.name == t) {
                    return err(r.line, format!("rule sends to unknown channel `{t}`"));
                }
            }
        }
        if let Some(d) = &cfg.digest {
            for t in &d.to {
                if !cfg.channels.iter().any(|c| &c.name == t) {
                    return err(d.line, format!("digest sends to unknown channel `{t}`"));
                }
            }
        }
        Ok(cfg)
    }

    fn parse_digest(&mut self, ln: usize, args: &[String]) -> Result<(), ConfigError> {
        if self.digest.is_some() {
            return err(ln, "only one digest directive is allowed");
        }
        let mut d = DigestCfg { schedule: Schedule::Every(3600), severities: vec![Severity::Info, Severity::Warn], lines: 30, to: vec![], line: ln };
        let mut seen_sched = false;
        for a in args {
            let Some((k, v)) = split_kv(a) else { return err(ln, format!("expected key=value, got `{a}`")) };
            match k {
                "every" | "at" if seen_sched => return err(ln, "give either every= or at=, once"),
                "every" => {
                    let s = parse_duration(v).or_else(|e| err(ln, format!("every: {e}")))?;
                    if s < 60 {
                        return err(ln, "every must be at least 1m");
                    }
                    d.schedule = Schedule::Every(s);
                    seen_sched = true;
                }
                "at" => {
                    let mut ts = Vec::new();
                    for t in v.split(',') {
                        match parse_hhmm(t) {
                            Some(m) if m < 1440 => ts.push(m),
                            _ => return err(ln, format!("bad time `{t}` in at= (HH:MM, 00:00-23:59)")),
                        }
                    }
                    ts.sort_unstable();
                    ts.dedup();
                    if ts.is_empty() {
                        return err(ln, "at= needs at least one time");
                    }
                    d.schedule = Schedule::At(ts);
                    seen_sched = true;
                }
                "severities" => {
                    d.severities.clear();
                    for s in v.split(',').filter(|s| !s.is_empty()) {
                        match Severity::parse(s) {
                            Some(x) if x != Severity::Fail => d.severities.push(x),
                            Some(_) => return err(ln, "fail is never held for a digest"),
                            None => return err(ln, format!("bad severity `{s}`")),
                        }
                    }
                }
                "lines" => d.lines = v.parse::<usize>().ok().filter(|n| (1..=500).contains(n)).ok_or_else(|| ConfigError { line: ln, msg: "lines must be 1..500".into() })?,
                "to" => d.to = v.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
                _ => return err(ln, format!("unknown digest key `{k}`")),
            }
        }
        if !seen_sched {
            return err(ln, "digest needs every=30m or at=08:00,18:00");
        }
        self.digest = Some(d);
        Ok(())
    }

    fn parse_flood(&mut self, ln: usize, args: &[String]) -> Result<(), ConfigError> {
        let mut f = FloodCfg::default();
        for a in args {
            if a == "off" {
                f.max_per_minute = 0;
                continue;
            }
            let Some((k, v)) = split_kv(a) else { return err(ln, format!("expected key=value or `off`, got `{a}`")) };
            match k {
                "max" => f.max_per_minute = v.parse().or_else(|_| err(ln, "max must be a whole number"))?,
                "cooldown" => f.cooldown_secs = parse_duration(v).or_else(|e| err(ln, format!("cooldown: {e}")))?,
                _ => return err(ln, format!("unknown flood key `{k}`")),
            }
        }
        self.flood = f;
        Ok(())
    }

    fn parse_set(&mut self, ln: usize, args: &[String]) -> Result<(), ConfigError> {
        for a in args {
            let Some((k, v)) = split_kv(a) else { return err(ln, format!("expected key=value, got `{a}`")) };
            let num = |what: &str| v.parse::<u64>().or_else(|_| err(ln, format!("{what} must be a whole number, got `{v}`")));
            match k {
                "timeout" => self.settings.timeout_secs = num("timeout")?.clamp(1, 600),
                "retries" => self.settings.retries = num("retries")?.min(10) as u32,
                "backoff" => self.settings.backoff_ms = num("backoff")?.min(60_000),
                "group" => self.settings.group_secs = parse_duration(v).or_else(|e| err(ln, format!("group: {e}")))?,
                "max_age_hours" => self.settings.max_age_hours = num("max_age_hours")?,
                "state_dir" => self.settings.state_dir = Some(v.to_string()),
                _ => return err(ln, format!("unknown setting `{k}`")),
            }
        }
        Ok(())
    }

    fn parse_channel(&mut self, ln: usize, args: &[String]) -> Result<(), ConfigError> {
        let Some((name, rest)) = args.split_first() else { return err(ln, "channel needs a name") };
        if !valid_name(name) {
            return err(ln, format!("bad channel name `{name}` (use a-z 0-9 - _)"));
        }
        if let Some(prev) = self.channels.iter().find(|c| &c.name == name) {
            return err(ln, format!("duplicate channel `{name}` (first defined on line {})", prev.line));
        }
        let mut opts = BTreeMap::new();
        for a in rest {
            let Some((k, v)) = split_kv(a) else { return err(ln, format!("expected key=value, got `{a}`")) };
            if opts.insert(k.to_string(), v.to_string()).is_some() {
                return err(ln, format!("option `{k}` given twice"));
            }
        }
        let kind = opts.remove("type").unwrap_or_else(|| name.clone());
        let required = match opts.remove("required").as_deref() {
            None | Some("no") | Some("false") => false,
            Some("yes") | Some("true") => true,
            Some(o) => return err(ln, format!("required must be yes or no, got `{o}`")),
        };
        let spec = ChannelSpec { name: name.clone(), kind, opts, required, line: ln };
        crate::channels::validate(&spec).map_err(|m| ConfigError { line: ln, msg: m })?;
        self.channels.push(spec);
        Ok(())
    }

    fn parse_rule(&mut self, ln: usize, args: &[String]) -> Result<(), ConfigError> {
        let mut r = Rule { app: None, tag: None, min: Severity::Info, to: vec![], line: ln };
        for a in args {
            let Some((k, v)) = split_kv(a) else { return err(ln, format!("expected key=value, got `{a}`")) };
            match k {
                "app" => r.app = Some(v.to_string()),
                "tag" => r.tag = Some(v.to_string()),
                "min" => match Severity::parse(v) {
                    Some(s) => r.min = s,
                    None => return err(ln, format!("bad severity `{v}` (ok, warn, fail, info)")),
                },
                "to" => r.to = v.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
                _ => return err(ln, format!("unknown rule key `{k}`")),
            }
        }
        if r.to.is_empty() {
            return err(ln, "rule needs to=channel[,channel]");
        }
        self.rules.push(r);
        Ok(())
    }

    fn parse_quiet(&mut self, ln: usize, args: &[String]) -> Result<(), ConfigError> {
        let Some((range, rest)) = args.split_first() else { return err(ln, "quiet needs a range like 22:00-07:00") };
        let bad = || ConfigError { line: ln, msg: format!("bad time range `{range}` (expected HH:MM-HH:MM, 24:00 allowed as end)") };
        let (a, b) = range.split_once('-').ok_or_else(bad)?;
        let (start, end) = (parse_hhmm(a).ok_or_else(bad)?, parse_hhmm(b).ok_or_else(bad)?);
        if start >= 1440 {
            return Err(bad());
        }
        if start == end {
            return err(ln, format!("empty time range `{range}`"));
        }
        let mut except = Vec::new();
        for a in rest {
            match split_kv(a) {
                Some(("except", v)) => {
                    for s in v.split(',').filter(|s| !s.is_empty()) {
                        match Severity::parse(s) {
                            Some(x) => except.push(x),
                            None => return err(ln, format!("bad severity `{s}` in except=")),
                        }
                    }
                }
                _ => return err(ln, format!("unknown quiet option `{a}`")),
            }
        }
        self.quiet.push(Quiet { start, end, except, line: ln });
        Ok(())
    }

    /// Names of the channels a message goes to. No rules at all = every channel.
    /// With rules: the union of the `to=` of every matching rule.
    pub fn select(&self, m: &Message) -> Vec<String> {
        self.select_as(m, m.severity)
    }

    /// Like `select`, but matched as if the message had `sev` (state changes route by the worse of old and new).
    pub fn select_as(&self, m: &Message, sev: Severity) -> Vec<String> {
        if self.rules.is_empty() {
            return self.channels.iter().map(|c| c.name.clone()).collect();
        }
        let mut out: Vec<String> = Vec::new();
        for r in &self.rules {
            if rule_matches(r, m, sev) {
                for t in &r.to {
                    if !out.contains(t) {
                        out.push(t.clone());
                    }
                }
            }
        }
        out
    }

    /// Is the message held back by quiet hours at this local time?
    pub fn quiet_holds(&self, severity: Severity, local_minutes: u32) -> bool {
        self.quiet.iter().any(|q| in_range(q.start, q.end, local_minutes) && !q.except.contains(&severity))
    }
}

/// `start..end` in minutes of the day; `start > end` wraps over midnight.
pub fn in_range(start: u32, end: u32, now: u32) -> bool {
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

fn rule_matches(r: &Rule, m: &Message, sev: Severity) -> bool {
    r.app.as_deref().is_none_or(|p| glob(p, &m.app)) && r.tag.as_deref().is_none_or(|p| glob(p, &m.tag)) && sev >= r.min
}

/// Case-insensitive glob with `*` and `?`.
pub fn glob(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let t: Vec<char> = s.to_lowercase().chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(text: &str) -> ConfigError {
        Config::parse(text).unwrap_err()
    }

    #[test]
    fn empty_file_and_comments_only_are_fine() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(Config::parse("\n# nothing\n   \n").unwrap(), Config::default());
    }

    #[test]
    fn unknown_directive_names_the_line() {
        let x = e("# c\n\nchanel telegram x=1\n");
        assert_eq!(x.line, 3);
        assert!(x.msg.contains("unknown directive `chanel`"), "{x}");
    }

    #[test]
    fn crlf_and_bom_are_accepted() {
        let c = Config::parse("\u{feff}set timeout=5\r\nchannel desktop\r\nrule to=desktop\r\n").unwrap();
        assert_eq!(c.settings.timeout_secs, 5);
        assert_eq!(c.channels.len(), 1);
        assert_eq!(c.rules[0].to, vec!["desktop"]);
    }

    #[test]
    fn duplicate_channel_is_an_error_with_both_lines() {
        let x = e("channel desktop\nchannel desktop\n");
        assert_eq!(x.line, 2);
        assert!(x.msg.contains("line 1"), "{x}");
    }

    #[test]
    fn bad_time_ranges() {
        for bad in ["25:00-07:00", "22:00-07:60", "22:00", "22:00-", "ab-cd", "24:00-01:00", "22:5-07:00", "24:30-01:00"] {
            let x = e(&format!("quiet {bad}\n"));
            assert_eq!(x.line, 1, "{bad}");
            assert!(x.msg.contains("bad time range"), "{bad}: {x}");
        }
        assert!(e("quiet 07:00-07:00").msg.contains("empty"));
    }

    #[test]
    fn quiet_hours_cross_midnight_and_allow_24_00() {
        let c = Config::parse("quiet 22:00-07:00 except=fail\n").unwrap();
        let at = |h: u32, m: u32, s| c.quiet_holds(s, h * 60 + m);
        assert!(at(22, 0, Severity::Info));
        assert!(at(23, 59, Severity::Warn));
        assert!(at(0, 0, Severity::Warn));
        assert!(at(6, 59, Severity::Warn));
        assert!(!at(7, 0, Severity::Warn), "end is exclusive");
        assert!(!at(21, 59, Severity::Warn));
        assert!(!at(23, 0, Severity::Fail), "except=fail passes");
        let d = Config::parse("quiet 22:00-24:00\nquiet 00:00-24:00 except=fail\n").unwrap();
        assert_eq!(d.quiet[0].end, 1440);
        assert!(d.quiet_holds(Severity::Ok, 23 * 60 + 59));
        assert!(d.quiet_holds(Severity::Ok, 0), "00:00-24:00 is the whole day");
        assert!(!d.quiet_holds(Severity::Fail, 12 * 60));
    }

    #[test]
    fn quoting_and_comments() {
        let c = Config::parse("channel hook type=webhook url=http://x body=\"{\\\"a\\\": \\\"{text}\\\"}\" # trailing\n").unwrap();
        assert_eq!(c.channels[0].opts["body"], "{\"a\": \"{text}\"}");
        assert!(e("channel hook type=webhook url=\"oops\n").msg.contains("unterminated"));
    }

    #[test]
    fn rule_to_unknown_channel_and_bad_keys() {
        assert!(e("rule to=nope\n").msg.contains("unknown channel `nope`"));
        assert!(e("channel desktop\nrule color=red to=desktop\n").msg.contains("unknown rule key"));
        assert!(e("channel desktop\nrule min=loud to=desktop\n").msg.contains("bad severity"));
        assert!(e("rule app=x\n").msg.contains("needs to="));
        assert!(e("set colour=1\n").msg.contains("unknown setting"));
        assert!(e("set timeout=abc\n").msg.contains("whole number"));
    }

    #[test]
    fn rules_select_by_app_min_and_tag_and_union() {
        let c = Config::parse(
            "channel desktop\nchannel ntfy topic=abcdef\nchannel webhook url=http://127.0.0.1:1/\n\
             rule app=judged min=warn to=ntfy\nrule app=judged* min=fail to=webhook\nrule app=smart-optimizer to=desktop\nrule tag=disk to=desktop\n",
        )
        .unwrap();
        let m = |app: &str, s, tag: &str| Message::new("t").app(app).severity(s).tag(tag);
        assert_eq!(c.select(&m("judged", Severity::Info, "")), Vec::<String>::new());
        assert_eq!(c.select(&m("judged", Severity::Warn, "")), vec!["ntfy"]);
        assert_eq!(c.select(&m("Judged-x", Severity::Fail, "")), vec!["webhook"]);
        assert_eq!(c.select(&m("judged", Severity::Fail, "")), vec!["ntfy", "webhook"]);
        assert_eq!(c.select(&m("smart-optimizer", Severity::Info, "")), vec!["desktop"]);
        assert_eq!(c.select(&m("other", Severity::Info, "disk")), vec!["desktop"]);
    }

    #[test]
    fn no_rules_means_every_channel() {
        let c = Config::parse("channel desktop\nchannel webhook url=http://127.0.0.1:1/\n").unwrap();
        assert_eq!(c.select(&Message::new("x")), vec!["desktop", "webhook"]);
    }

    #[test]
    fn durations_need_units() {
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("2h"), Ok(7200));
        assert!(parse_duration("10").unwrap_err().contains("unit"));
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("5w").is_err());
    }

    #[test]
    fn digest_and_flood_directives() {
        let c = Config::parse("channel desktop\ndigest at=18:00,08:00 severities=info lines=5 to=desktop\nflood max=5 cooldown=2m\n").unwrap();
        let d = c.digest.unwrap();
        assert_eq!(d.schedule, Schedule::At(vec![480, 1080]));
        assert_eq!((d.severities, d.lines), (vec![Severity::Info], 5));
        assert_eq!(c.flood, FloodCfg { max_per_minute: 5, cooldown_secs: 120 });
        assert_eq!(Config::parse("flood off\n").unwrap().flood.max_per_minute, 0);
        for bad in [
            "digest\n",
            "digest every=30m at=08:00\n",
            "digest every=30s\n",
            "digest at=24:00\n",
            "digest every=1h severities=fail\n",
            "digest every=1h\ndigest every=2h\n",
            "digest every=1h to=ghost\n",
            "flood max=x\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn glob_cases() {
        assert!(glob("*", ""));
        assert!(glob("a*c", "abbbc"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "ac"));
        assert!(glob("JUDGED", "judged"));
        assert!(!glob("judged", "judged2"));
    }
}
