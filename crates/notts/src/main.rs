//! The `notts` command line. The contract (flags, exit codes, JSON) is frozen in the README.

use notts::config::parse_duration;
use notts::serve::{default_socket, ServeOpts, Server};
use notts::{Init, Message, Notts, Report, SendOptions, Severity, CONTRACT, VERSION};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const HELP: &str = "\
notts: one message, every channel you enabled.

usage:
  notts [GLOBAL] send --title T [--body B | --body-file FILE|- | < stdin] [--app NAME]
                      [--severity ok|warn|fail|info] [--link URL] [--tag T] [--key K]
                      [--state ok|warn|fail] [--to ch1,ch2] [--dry-run] [--no-group] [--json]
                      [--attach FILE [--attach-name NAME]] (up to 3, in pairs)
  notts [GLOBAL] flush                  retry the outbox
  notts [GLOBAL] status [--json]        channels (configured or why not), outbox size
  notts [GLOBAL] log [--app A] [--since 2h] [--grouped]
  notts [GLOBAL] digest [--now] [--dry-run] [--json]
  notts [GLOBAL] hook                   send one message from finedesktop's FD_* environment
  notts [GLOBAL] watch                  forward desktop notifications (needs busctl)
  notts [GLOBAL] serve [--socket PATH] [--listen 127.0.0.1:PORT --token-file FILE]
                       [--queue N] [--workers N] [--rate PER_MIN] [--max-body BYTES]
  notts --version | --help

global:  --config FILE (default $NOTTS_CONFIG or $XDG_CONFIG_HOME/notts/notts.conf)
         --state-dir DIR

exit codes: 0 delivered (or deliberately grouped/held/suppressed), 1 nothing delivered,
            2 bad usage or config, 3 queued in the outbox only
";

struct Args {
    rest: Vec<String>,
}

impl Args {
    /// `--name value` or `--name=value`; removes it.
    fn value(&mut self, name: &str) -> Result<Option<String>, String> {
        let eq = format!("{name}=");
        let mut i = 0;
        let mut found = None;
        while i < self.rest.len() {
            if self.rest[i] == name {
                if i + 1 >= self.rest.len() {
                    return Err(format!("{name} needs a value"));
                }
                found = Some(self.rest[i + 1].clone());
                self.rest.drain(i..i + 2);
                continue;
            }
            if let Some(v) = self.rest[i].strip_prefix(&eq) {
                found = Some(v.to_string());
                self.rest.remove(i);
                continue;
            }
            i += 1;
        }
        Ok(found)
    }
    /// Every occurrence of a repeatable option, in order.
    fn values(&mut self, name: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        while let Some(v) = self.take_first(name)? {
            out.push(v);
        }
        Ok(out)
    }
    fn take_first(&mut self, name: &str) -> Result<Option<String>, String> {
        let eq = format!("{name}=");
        for i in 0..self.rest.len() {
            if self.rest[i] == name {
                if i + 1 >= self.rest.len() {
                    return Err(format!("{name} needs a value"));
                }
                let v = self.rest[i + 1].clone();
                self.rest.drain(i..i + 2);
                return Ok(Some(v));
            }
            if let Some(v) = self.rest[i].strip_prefix(&eq) {
                let v = v.to_string();
                self.rest.remove(i);
                return Ok(Some(v));
            }
        }
        Ok(None)
    }
    fn flag(&mut self, name: &str) -> bool {
        let before = self.rest.len();
        self.rest.retain(|a| a != name);
        self.rest.len() != before
    }
    fn done(&self) -> Result<(), String> {
        match self.rest.first() {
            None => Ok(()),
            Some(a) => Err(format!("unexpected argument `{a}` (see notts --help)")),
        }
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("notts: {msg}");
    ExitCode::from(2)
}

fn num(v: Option<String>, name: &str, default: usize) -> Result<usize, String> {
    match v {
        None => Ok(default),
        Some(s) => s.parse().map_err(|_| format!("{name} must be a number")),
    }
}

fn main() -> ExitCode {
    let mut args = Args { rest: std::env::args().skip(1).collect() };
    if args.rest.iter().any(|a| a == "--version" || a == "-V") {
        println!("notts {VERSION} (contract {CONTRACT})");
        return ExitCode::SUCCESS;
    }
    if args.rest.is_empty() || args.rest.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return if args.rest.is_empty() { ExitCode::from(2) } else { ExitCode::SUCCESS };
    }
    let (config, state_dir) = match (args.value("--config"), args.value("--state-dir")) {
        (Ok(c), Ok(s)) => (c, s),
        (Err(e), _) | (_, Err(e)) => return usage(&e),
    };
    if args.rest.is_empty() {
        return usage("missing command (see notts --help)");
    }
    let cmd = args.rest.remove(0);
    if !["send", "flush", "status", "log", "digest", "hook", "watch", "serve"].contains(&cmd.as_str()) {
        return usage(&format!("unknown command `{cmd}` (see notts --help)"));
    }
    let path = config.map(PathBuf::from).or_else(|| std::env::var_os("NOTTS_CONFIG").filter(|v| !v.is_empty()).map(PathBuf::from)).unwrap_or_else(notts::default_config_path);
    let init = Init { state_dir: state_dir.map(PathBuf::from), ..Init::default() };
    let n = match Notts::from_config_with(&path, init) {
        Ok(n) => n,
        Err(e) => return usage(&format!("{e}")),
    };
    let r = match cmd.as_str() {
        "send" => cmd_send(&n, args),
        "flush" => cmd_flush(&n, args),
        "status" => cmd_status(&n, args),
        "log" => cmd_log(&n, args),
        "digest" => cmd_digest(&n, args),
        "hook" => cmd_hook(&n, args),
        "watch" => cmd_watch(&n, args),
        _ => cmd_serve(n, args),
    };
    match r {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => usage(&e),
    }
}

fn print_report(n: &Notts, r: &Report, json: bool) {
    if json {
        println!("{}", r.to_json());
        return;
    }
    for c in &r.results {
        println!("{}: {}{}", c.name, c.status.as_str(), if c.detail.is_empty() { String::new() } else { format!(" ({})", c.detail) });
    }
    if r.results.is_empty() || r.disposition != notts::Disposition::Sent {
        println!("{}{}", r.disposition.as_str(), if r.reason.is_empty() { String::new() } else { format!(": {}", n.mask(&r.reason)) });
    }
}

const MAX_STDIN: u64 = 4 * 1024 * 1024;

/// All of stdin; more than the cap is an error, never a silent cut.
fn read_stdin() -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    std::io::stdin().take(MAX_STDIN + 1).read_to_end(&mut buf).map_err(|e| format!("cannot read stdin ({:?})", e.kind()))?;
    if buf.len() as u64 > MAX_STDIN {
        return Err(format!("stdin is larger than {MAX_STDIN} bytes; send the text as a file with --attach instead"));
    }
    Ok(buf)
}

fn cmd_send(n: &Notts, mut a: Args) -> Result<i32, String> {
    let title = a.value("--title")?;
    let body = a.value("--body")?;
    let body_file = a.value("--body-file")?;
    let attach = a.values("--attach")?;
    let attach_names = a.values("--attach-name")?;
    let app = a.value("--app")?;
    let sev = a.value("--severity")?;
    let link = a.value("--link")?;
    let tag = a.value("--tag")?;
    let key = a.value("--key")?;
    let state = a.value("--state")?;
    let to = a.value("--to")?;
    let dry = a.flag("--dry-run");
    let no_group = a.flag("--no-group");
    let json = a.flag("--json");
    a.done()?;
    let severity = match sev {
        None => Severity::Info,
        Some(s) => Severity::parse(&s).ok_or("--severity must be ok, warn, fail or info")?,
    };
    let mut m = Message::new(title.clone().unwrap_or_default()).severity(severity).app(app.unwrap_or_default()).tag(tag.unwrap_or_default());
    if body.is_some() && body_file.is_some() {
        return Err("give --body or --body-file, not both".into());
    }
    match (body, body_file) {
        (Some(b), _) => m = m.body(b),
        (None, Some(f)) => {
            let buf = if f == "-" {
                read_stdin()?
            } else {
                let p = notts::secret::expand_tilde(&f);
                let meta = std::fs::metadata(&p).map_err(|e| format!("cannot read --body-file {f} ({:?})", e.kind()))?;
                if !meta.is_file() || meta.len() > MAX_STDIN {
                    return Err(format!("--body-file {f} must be a regular file of at most {MAX_STDIN} bytes"));
                }
                std::fs::read(&p).map_err(|e| format!("cannot read --body-file {f} ({:?})", e.kind()))?
            };
            m = m.body_bytes(&buf);
        }
        (None, None) if !std::io::stdin().is_terminal() => {
            let buf = read_stdin()?;
            m = m.body_bytes(&buf);
        }
        (None, None) => {}
    }
    if attach.len() > notts::MAX_ATTACHMENTS {
        return Err(format!("at most {} --attach files", notts::MAX_ATTACHMENTS));
    }
    if attach_names.len() > attach.len() {
        return Err("--attach-name needs a matching --attach".into());
    }
    for (i, path) in attach.iter().enumerate() {
        m = m.attach(notts::Attachment::from_path(path, attach_names.get(i).map(|s| s.as_str()))?);
    }
    if let Some(l) = link {
        m = m.link(l);
    }
    if title.as_deref().unwrap_or("").is_empty() && m.body.is_empty() {
        return Err("need --title or a body (--body or stdin)".into());
    }
    let mut o = SendOptions::new().dry_run(dry).no_group(no_group);
    if let Some(k) = key {
        o = o.key(k);
    }
    if let Some(s) = state {
        match Severity::parse(&s) {
            Some(Severity::Info) | None => return Err("--state must be ok, warn or fail".into()),
            Some(x) => o = o.state(x),
        }
    }
    if let Some(t) = to {
        o = o.to(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
    }
    let r = n.send(&m, &o);
    print_report(n, &r, json);
    Ok(r.exit_code)
}

fn cmd_flush(n: &Notts, a: Args) -> Result<i32, String> {
    a.done()?;
    let f = n.flush();
    println!("delivered {}, remaining {}, buried {}, torn lines {}", f.delivered, f.remaining, f.buried, f.torn_lines);
    for note in &f.notes {
        println!("  {note}");
    }
    Ok(f.exit_code())
}

fn cmd_status(n: &Notts, mut a: Args) -> Result<i32, String> {
    let json = a.flag("--json");
    a.done()?;
    let st = n.channel_status();
    if json {
        let rows: Vec<String> = st
            .iter()
            .map(|(name, kind, r)| {
                format!(
                    "{{\"name\":{},\"kind\":{},\"configured\":{},\"detail\":{}}}",
                    serde_json::to_string(name).unwrap_or_default(),
                    serde_json::to_string(kind).unwrap_or_default(),
                    r.is_ok(),
                    serde_json::to_string(&r.as_ref().err().cloned().unwrap_or_default()).unwrap_or_default()
                )
            })
            .collect();
        println!("{{\"notts_version\":\"{VERSION}\",\"contract\":{CONTRACT},\"outbox\":{},\"channels\":[{}]}}", n.outbox_len(), rows.join(","));
    } else {
        for (name, kind, r) in &st {
            match r {
                Ok(()) => println!("{name} ({kind}): ready"),
                Err(e) => println!("{name} ({kind}): not configured, {e}"),
            }
        }
        println!("outbox: {} waiting", n.outbox_len());
    }
    Ok(0)
}

fn cmd_log(n: &Notts, mut a: Args) -> Result<i32, String> {
    let app = a.value("--app")?.map(|s| s.to_ascii_lowercase());
    let since = a.value("--since")?;
    let grouped = a.flag("--grouped");
    a.done()?;
    let cutoff = match since {
        None => 0,
        Some(s) => {
            let secs = parse_duration(&s).map_err(|e| format!("--since: {e}"))?;
            {
                use notts::Clock;
                notts::SystemClock.unix()
            }
            .saturating_sub(secs)
        }
    };
    for e in n.history() {
        if e.at < cutoff || app.as_ref().is_some_and(|x| e.app.to_ascii_lowercase() != *x) || (grouped && e.decision != "grouped") {
            continue;
        }
        println!(
            "{} {:<10} {:<12} {:<5} {} [{}]{}",
            notts::clock::format_local(e.at),
            e.decision,
            if e.app.is_empty() { "-" } else { &e.app },
            e.severity,
            e.title,
            e.key,
            if e.reason.is_empty() { String::new() } else { format!(" {}", e.reason) }
        );
    }
    Ok(0)
}

fn cmd_digest(n: &Notts, mut a: Args) -> Result<i32, String> {
    let now = a.flag("--now");
    let dry = a.flag("--dry-run");
    let json = a.flag("--json");
    a.done()?;
    match n.digest(now, dry) {
        Some(r) => {
            print_report(n, &r, json);
            Ok(r.exit_code)
        }
        None => {
            if !json {
                println!("nothing to send");
            }
            Ok(0)
        }
    }
}

/// finedesktop's `on-notify` hook: FD_TEXT, FD_APP, FD_URGENCY, FD_TAG.
fn cmd_hook(n: &Notts, a: Args) -> Result<i32, String> {
    a.done()?;
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let text = env("FD_TEXT");
    if text.is_empty() {
        return Err("FD_TEXT is empty: run this from finedesktop's on-notify hook".into());
    }
    let app = env("FD_APP");
    if app == "notts" {
        return Ok(0);
    }
    let m = Message::new(text).app(if app.is_empty() { "finedesktop".to_string() } else { app }).tag(env("FD_TAG")).severity(notts::watch::severity_from_urgency(&env("FD_URGENCY")));
    Ok(n.send(&m, &SendOptions::new()).exit_code)
}

fn cmd_watch(n: &Notts, a: Args) -> Result<i32, String> {
    a.done()?;
    use std::io::BufRead;
    let mut child = std::process::Command::new("busctl")
        .args(["--user", "monitor", "--json=short", "org.freedesktop.Notifications"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start busctl ({:?}); is systemd's busctl installed?", e.kind()))?;
    let out = child.stdout.take().ok_or("no busctl output")?;
    for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
        if let Some(m) = notts::watch::parse_notify(&line) {
            let _ = n.send(&m, &SendOptions::new());
        }
    }
    let _ = child.wait();
    Err("busctl ended; is a session bus running?".into())
}

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn cmd_serve(n: Notts, mut a: Args) -> Result<i32, String> {
    let d = ServeOpts::default();
    let socket = a.value("--socket")?;
    let no_socket = a.flag("--no-socket");
    let listen = a.value("--listen")?;
    let token_file = a.value("--token-file")?;
    let queue = num(a.value("--queue")?, "--queue", d.queue)?;
    let workers = num(a.value("--workers")?, "--workers", d.workers)?;
    let rate = num(a.value("--rate")?, "--rate", d.rate_per_min as usize)?;
    let max_body = num(a.value("--max-body")?, "--max-body", d.max_body)?;
    a.done()?;
    let socket = if no_socket { None } else { socket.map(PathBuf::from).or_else(default_socket) };
    let token = match token_file {
        Some(p) => Some(notts::secret::read_secret_file(&p)?),
        None => None,
    };
    let opts = ServeOpts { socket, listen, token, queue, workers, rate_per_min: rate as u32, max_body, ..d };
    let stop = Arc::new(AtomicBool::new(false));
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
    }
    let n = Arc::new(n);
    let server = Server::start(n.clone(), opts.clone(), stop.clone())?;
    if let Some(s) = &opts.socket {
        eprintln!("notts serve: listening on {}", s.display());
    }
    if let Some(t) = server.tcp_addr() {
        eprintln!("notts serve: listening on {t} (bearer token required)");
    }
    let _ = std::io::stderr().flush();
    while !STOP.load(Ordering::SeqCst) && !server.stopped() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    server.shutdown();
    let f = n.flush();
    eprintln!("notts serve: stopped, outbox holds {}", f.remaining);
    Ok(0)
}
