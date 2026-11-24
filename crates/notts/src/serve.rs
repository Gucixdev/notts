//! `notts serve`: the same contract as the CLI, for long-running services, over a Unix socket
//! (mode 600) and optionally 127.0.0.1:PORT with a bearer token. Plain HTTP/1.1, one request per
//! connection, no async runtime.
//!
//!   POST /v1/send    JSON in (app severity title body link tag key state to dry_run no_group),
//!                    the CLI's `--json` object out. 200 = delivered, 202 = queued/held in outbox,
//!                    502 = nothing delivered, 400/401/404/405/411/413/429/503 for the usual reasons.
//!   GET  /v1/health  {"status":"ok",...queue depth}
//!   GET  /v1/outbox  {"count":N}
//!
//! Back-pressure: a bounded job queue and a bounded number of connections; when full the answer is
//! 429 with Retry-After, never unbounded growth. A per-app token bucket and a per-app in-flight cap
//! keep one noisy service from starving the others. On shutdown queued jobs go to the outbox.

use crate::engine::{Disposition, Notts, Report, SendOptions, CONTRACT, VERSION};
use crate::message::{Message, Severity};
use crate::secret::Secret;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ServeOpts {
    pub socket: Option<PathBuf>,
    pub listen: Option<String>,
    pub token: Option<Secret>,
    pub queue: usize,
    pub workers: usize,
    pub rate_per_min: u32,
    pub max_body: usize,
    pub max_conns: usize,
    pub flush_every: Duration,
}

impl Default for ServeOpts {
    fn default() -> Self {
        ServeOpts { socket: None, listen: None, token: None, queue: 64, workers: 4, rate_per_min: 120, max_body: 12 * 1024 * 1024, max_conns: 128, flush_every: Duration::from_secs(300) }
    }
}

pub fn default_socket() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(|d| PathBuf::from(d).join("notts.sock"))
}

/// Token bucket per app: `rate` messages per minute, burst = `rate`.
pub struct RateLimiter {
    rate: f64,
    buckets: HashMap<String, (f64, u64)>,
}

const MAX_APPS: usize = 1024;

impl RateLimiter {
    pub fn new(rate_per_min: u32) -> RateLimiter {
        RateLimiter { rate: rate_per_min as f64, buckets: HashMap::new() }
    }
    /// `Ok` or the seconds to wait.
    pub fn allow(&mut self, app: &str, now_ms: u64) -> Result<(), u64> {
        if self.rate <= 0.0 {
            return Ok(());
        }
        let key = if self.buckets.contains_key(app) || self.buckets.len() < MAX_APPS { app } else { "*overflow*" };
        let b = self.buckets.entry(key.to_string()).or_insert((self.rate, now_ms));
        let elapsed = now_ms.saturating_sub(b.1) as f64 / 60_000.0;
        b.0 = (b.0 + elapsed * self.rate).min(self.rate);
        b.1 = now_ms;
        if b.0 >= 1.0 {
            b.0 -= 1.0;
            Ok(())
        } else {
            Err((((1.0 - b.0) / self.rate) * 60.0).ceil().max(1.0) as u64)
        }
    }
}

struct Job {
    msg: Message,
    opts: SendOptions,
    app: String,
    reply: SyncSender<Report>,
}

struct Shared {
    notts: Arc<Notts>,
    opts: ServeOpts,
    stop: Arc<AtomicBool>,
    limiter: Mutex<RateLimiter>,
    inflight: Mutex<HashMap<String, usize>>,
    per_app_cap: usize,
    queued: AtomicUsize,
    conns: AtomicUsize,
    tx: SyncSender<Job>,
    start: Instant,
}

pub struct Server {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    socket: Option<PathBuf>,
    tcp: Option<SocketAddr>,
}

fn parse_loopback(s: &str) -> Result<SocketAddr, String> {
    let s = s.replace("localhost", "127.0.0.1");
    let a: SocketAddr = s.parse().map_err(|_| format!("`{s}` is not an ip:port address"))?;
    if !a.ip().is_loopback() {
        return Err(format!("refusing to listen on {}: notts serve only binds loopback (127.0.0.1 or ::1); use the unix socket or a reverse proxy you control", a.ip()));
    }
    Ok(a)
}

impl Server {
    pub fn start(notts: Arc<Notts>, opts: ServeOpts, stop: Arc<AtomicBool>) -> Result<Server, String> {
        if opts.socket.is_none() && opts.listen.is_none() {
            return Err("nothing to listen on: give --socket PATH (default $XDG_RUNTIME_DIR/notts.sock) or --listen 127.0.0.1:PORT".into());
        }
        if opts.queue == 0 || opts.workers == 0 {
            return Err("--queue and --workers must be at least 1".into());
        }
        let tcp_listener = match &opts.listen {
            Some(l) => {
                let addr = parse_loopback(l)?;
                if opts.token.is_none() {
                    return Err("--listen needs --token-file (a mode-600 file holding the bearer token)".into());
                }
                let t = TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr} ({:?})", e.kind()))?;
                t.set_nonblocking(true).map_err(|e| format!("{:?}", e.kind()))?;
                Some(t)
            }
            None => None,
        };
        let unix_listener = match &opts.socket {
            Some(p) => Some(bind_unix(p)?),
            None => None,
        };
        let (tx, rx) = sync_channel::<Job>(opts.queue);
        let rx = Arc::new(Mutex::new(rx));
        let shared = Arc::new(Shared {
            per_app_cap: (opts.queue / 2).max(1),
            limiter: Mutex::new(RateLimiter::new(opts.rate_per_min)),
            inflight: Mutex::new(HashMap::new()),
            queued: AtomicUsize::new(0),
            conns: AtomicUsize::new(0),
            start: Instant::now(),
            notts,
            opts: opts.clone(),
            stop,
            tx,
        });
        let tcp = tcp_listener.as_ref().and_then(|l| l.local_addr().ok());
        let mut threads = Vec::new();
        for _ in 0..opts.workers {
            let (s, r) = (shared.clone(), rx.clone());
            threads.push(std::thread::spawn(move || worker(s, r)));
        }
        {
            let s = shared.clone();
            threads.push(std::thread::spawn(move || acceptor(s, tcp_listener, unix_listener)));
        }
        {
            let s = shared.clone();
            threads.push(std::thread::spawn(move || housekeeping(s)));
        }
        Ok(Server { shared, threads, socket: opts.socket.clone(), tcp })
    }

    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp
    }

    pub fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::SeqCst)
    }

    /// Stop accepting, park queued jobs in the outbox, wait for running ones, remove the socket.
    pub fn shutdown(self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for t in self.threads {
            let _ = t.join();
        }
        let t0 = Instant::now();
        while self.shared.conns.load(Ordering::SeqCst) > 0 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(p) = &self.socket {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn bind_unix(p: &Path) -> Result<UnixListener, String> {
    if p.exists() {
        if UnixStream::connect(p).is_ok() {
            return Err(format!("another notts serve already listens on {}", p.display()));
        }
        // a stale socket from a crashed run: ours to replace
        std::fs::remove_file(p).map_err(|e| format!("cannot replace stale socket {} ({:?})", p.display(), e.kind()))?;
    }
    // Bind inside a private (mode 700) directory, tighten the socket, then rename it into place:
    // nobody else can reach it at any moment, and no process-wide umask is touched.
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let stage = parent.join(format!(".notts-sock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&stage).map_err(|e| format!("cannot create {} ({:?})", stage.display(), e.kind()))?;
    }
    let tmp = stage.join("s");
    let bound = UnixListener::bind(&tmp)
        .map_err(|e| format!("cannot bind {} ({:?})", p.display(), e.kind()))
        .and_then(|l| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map(|_| l).map_err(|e| format!("cannot chmod socket ({:?})", e.kind())))
        .and_then(|l| std::fs::rename(&tmp, p).map(|_| l).map_err(|e| format!("cannot move socket to {} ({:?})", p.display(), e.kind())));
    let _ = std::fs::remove_dir_all(&stage);
    let l = bound?;
    l.set_nonblocking(true).map_err(|e| format!("{:?}", e.kind()))?;
    Ok(l)
}

fn acceptor(s: Arc<Shared>, tcp: Option<TcpListener>, unix: Option<UnixListener>) {
    while !s.stop.load(Ordering::SeqCst) {
        let mut idle = true;
        if let Some(l) = &tcp {
            if let Ok((c, _)) = l.accept() {
                idle = false;
                let _ = c.set_nonblocking(false);
                spawn_conn(&s, Conn::Tcp(c));
            }
        }
        if let Some(l) = &unix {
            if let Ok((c, _)) = l.accept() {
                idle = false;
                let _ = c.set_nonblocking(false);
                spawn_conn(&s, Conn::Unix(c));
            }
        }
        if idle {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Conn {
    fn is_tcp(&self) -> bool {
        matches!(self, Conn::Tcp(_))
    }
    fn timeouts(&self) {
        let d = Some(Duration::from_secs(10));
        match self {
            Conn::Tcp(c) => {
                let _ = c.set_read_timeout(d);
                let _ = c.set_write_timeout(d);
            }
            Conn::Unix(c) => {
                let _ = c.set_read_timeout(d);
                let _ = c.set_write_timeout(d);
            }
        }
    }
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(c) => c.read(b),
            Conn::Unix(c) => c.read(b),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(c) => c.write(b),
            Conn::Unix(c) => c.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Tcp(c) => c.flush(),
            Conn::Unix(c) => c.flush(),
        }
    }
}

fn spawn_conn(s: &Arc<Shared>, mut c: Conn) {
    if s.conns.fetch_add(1, Ordering::SeqCst) >= s.opts.max_conns {
        s.conns.fetch_sub(1, Ordering::SeqCst);
        let _ = respond(&mut c, 429, &err_json("too many connections"), Some(1));
        return;
    }
    let s2 = s.clone();
    std::thread::spawn(move || {
        c.timeouts();
        handle(&s2, &mut c);
        s2.conns.fetch_sub(1, Ordering::SeqCst);
    });
}

fn err_json(m: &str) -> String {
    format!("{{\"notts_version\":{},\"contract\":{},\"error\":{}}}", jq(VERSION), CONTRACT, jq(m))
}

fn jq(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn respond(c: &mut Conn, code: u16, body: &str, retry_after: Option<u64>) -> std::io::Result<()> {
    let text = match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut h = format!("HTTP/1.1 {code} {text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if let Some(r) = retry_after {
        h.push_str(&format!("Retry-After: {r}\r\n"));
    }
    if code == 401 {
        h.push_str("WWW-Authenticate: Bearer\r\n");
    }
    h.push_str("\r\n");
    c.write_all(h.as_bytes())?;
    c.write_all(body.as_bytes())?;
    c.flush()
}

struct Req {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

enum ReadErr {
    Status(u16, &'static str),
    Closed,
}

fn read_request(c: &mut Conn, max_body: usize) -> Result<Req, ReadErr> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16 * 1024 {
            return Err(ReadErr::Status(431, "headers too large"));
        }
        match c.read(&mut tmp) {
            Ok(0) | Err(_) => return Err(ReadErr::Closed),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err(ReadErr::Status(501, "chunked bodies are not supported, send Content-Length"));
    }
    let len = match headers.get("content-length") {
        Some(v) => v.parse::<usize>().map_err(|_| ReadErr::Status(400, "bad Content-Length"))?,
        None if method == "POST" => return Err(ReadErr::Status(411, "Content-Length required")),
        None => 0,
    };
    if len > max_body {
        return Err(ReadErr::Status(413, "body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        match c.read(&mut tmp) {
            Ok(0) | Err(_) => return Err(ReadErr::Closed),
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
        if body.len() > max_body {
            return Err(ReadErr::Status(413, "body too large"));
        }
    }
    body.truncate(len);
    Ok(Req { method, path, headers, body })
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut d = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().min(b.len()) {
        d |= a[i] ^ b[i];
    }
    d == 0
}

fn handle(s: &Arc<Shared>, c: &mut Conn) {
    let req = match read_request(c, s.opts.max_body) {
        Ok(r) => r,
        Err(ReadErr::Status(code, m)) => {
            let _ = respond(c, code, &err_json(m), None);
            return;
        }
        Err(ReadErr::Closed) => return,
    };
    if c.is_tcp() {
        let ok = match (&s.opts.token, req.headers.get("authorization")) {
            (Some(t), Some(h)) => h.strip_prefix("Bearer ").is_some_and(|got| ct_eq(got.trim().as_bytes(), t.expose().as_bytes())),
            _ => false,
        };
        if !ok {
            let _ = respond(c, 401, &err_json("missing or wrong bearer token"), None);
            return;
        }
    }
    if s.stop.load(Ordering::SeqCst) {
        let _ = respond(c, 503, &err_json("shutting down"), Some(5));
        return;
    }
    let path = req.path.split('?').next().unwrap_or("");
    let (code, body, retry) = match (req.method.as_str(), path) {
        ("GET", "/v1/health") => (
            200,
            format!(
                "{{\"notts_version\":{},\"contract\":{},\"status\":\"ok\",\"queue_depth\":{},\"queue_capacity\":{},\"uptime_s\":{}}}",
                jq(VERSION),
                CONTRACT,
                s.queued.load(Ordering::SeqCst),
                s.opts.queue,
                s.start.elapsed().as_secs()
            ),
            None,
        ),
        ("GET", "/v1/outbox") => (200, format!("{{\"notts_version\":{},\"contract\":{},\"count\":{}}}", jq(VERSION), CONTRACT, s.notts.outbox_len()), None),
        ("POST", "/v1/send") => send(s, &req.body, !c.is_tcp()),
        (_, "/v1/health") | (_, "/v1/outbox") | (_, "/v1/send") => (405, err_json("method not allowed"), None),
        _ => (404, err_json("not found"), None),
    };
    let _ = respond(c, code, &body, retry);
}

fn field<'a>(v: &'a serde_json::Value, k: &str) -> Result<Option<&'a str>, String> {
    match v.get(k) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("`{k}` must be a string")),
    }
}

fn flag(v: &serde_json::Value, k: &str) -> Result<bool, String> {
    match v.get(k) {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("`{k}` must be true or false")),
    }
}

/// JSON request body -> message and options (the same fields as the CLI flags).
/// `allow_paths`: attachments by local path are only honoured for clients on the unix socket (same user).
pub fn parse_send(body: &[u8], allow_paths: bool) -> Result<(Message, SendOptions), String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| "body is not valid JSON".to_string())?;
    if !v.is_object() {
        return Err("body must be a JSON object".into());
    }
    let title = field(&v, "title")?.unwrap_or("");
    let text = field(&v, "body")?.unwrap_or("");
    if title.is_empty() && text.is_empty() {
        return Err("need a title or a body".into());
    }
    let sev = match field(&v, "severity")? {
        None => Severity::Info,
        Some(s) => Severity::parse(s).ok_or("severity must be ok, warn, fail or info")?,
    };
    let mut m = Message::new(title).body(text).severity(sev).app(field(&v, "app")?.unwrap_or("")).tag(field(&v, "tag")?.unwrap_or(""));
    if let Some(l) = field(&v, "link")? {
        m = m.link(l);
    }
    match v.get("attachments") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Array(list)) => {
            if list.len() > crate::MAX_ATTACHMENTS {
                return Err(format!("at most {} attachments", crate::MAX_ATTACHMENTS));
            }
            for a in list {
                let name = a.get("name").and_then(|x| x.as_str());
                let att = match (a.get("path").and_then(|x| x.as_str()), a.get("base64").and_then(|x| x.as_str())) {
                    (Some(p), None) => {
                        if !allow_paths {
                            return Err("attachments by `path` are only accepted on the unix socket; send name + base64 over TCP".into());
                        }
                        crate::Attachment::from_path(p, name)?
                    }
                    (None, Some(b)) => {
                        let data = crate::text::base64_decode(b).ok_or("attachment `base64` is not valid base64")?;
                        if data.len() > crate::MAX_ATTACHMENT {
                            return Err(format!("attachment is {} bytes, the limit is {}", data.len(), crate::MAX_ATTACHMENT));
                        }
                        crate::Attachment::new(name.ok_or("an attachment with `base64` needs a `name`")?, data)
                    }
                    _ => return Err("each attachment needs exactly one of `path` or `base64` (with `name`)".into()),
                };
                m = m.attach(att);
            }
        }
        Some(_) => return Err("`attachments` must be a list".into()),
    }
    let mut o = SendOptions::new().dry_run(flag(&v, "dry_run")?).no_group(flag(&v, "no_group")?);
    if let Some(k) = field(&v, "key")? {
        o = o.key(k);
    }
    if let Some(st) = field(&v, "state")? {
        match Severity::parse(st) {
            Some(Severity::Info) | None => return Err("state must be ok, warn or fail".into()),
            Some(x) => o = o.state(x),
        }
    }
    match v.get("to") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Array(a)) => {
            let names: Option<Vec<String>> = a.iter().map(|x| x.as_str().map(String::from)).collect();
            o = o.to(names.ok_or("`to` must be a list of channel names")?);
        }
        Some(_) => return Err("`to` must be a list of channel names".into()),
    }
    Ok((m, o))
}

fn send(s: &Arc<Shared>, body: &[u8], allow_paths: bool) -> (u16, String, Option<u64>) {
    let (msg, opts) = match parse_send(body, allow_paths) {
        Ok(x) => x,
        Err(e) => return (400, err_json(&e), None),
    };
    let app = if msg.app.is_empty() { "-".to_string() } else { msg.app.to_ascii_lowercase() };
    let now_ms = s.start.elapsed().as_millis() as u64;
    if let Ok(mut l) = s.limiter.lock() {
        if let Err(wait) = l.allow(&app, now_ms) {
            return (429, err_json("per-app rate limit exceeded"), Some(wait));
        }
    }
    {
        let mut inf = match s.inflight.lock() {
            Ok(g) => g,
            Err(_) => return (503, err_json("internal error"), None),
        };
        let n = inf.entry(app.clone()).or_insert(0);
        if *n >= s.per_app_cap {
            return (429, err_json("too many of this app's messages in flight"), Some(1));
        }
        *n += 1;
    }
    let release = |s: &Shared, app: &str| {
        if let Ok(mut inf) = s.inflight.lock() {
            if let Some(n) = inf.get_mut(app) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    inf.remove(app);
                }
            }
        }
    };
    let (rtx, rrx) = sync_channel::<Report>(1);
    s.queued.fetch_add(1, Ordering::SeqCst);
    match s.tx.try_send(Job { msg, opts, app: app.clone(), reply: rtx }) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            s.queued.fetch_sub(1, Ordering::SeqCst);
            release(s, &app);
            return (429, err_json("queue full"), Some(1));
        }
        Err(TrySendError::Disconnected(_)) => {
            s.queued.fetch_sub(1, Ordering::SeqCst);
            release(s, &app);
            return (503, err_json("shutting down"), Some(5));
        }
    }
    let cfg = s.notts.config();
    let wait = Duration::from_secs((cfg.settings.timeout_secs + 1) * (cfg.settings.retries as u64 + 2) + 60);
    match rrx.recv_timeout(wait) {
        Ok(r) => {
            let code = match (r.exit_code, r.disposition) {
                (0, _) => 200,
                (3, _) => 202,
                _ => 502,
            };
            (code, r.to_json(), None)
        }
        Err(_) => (503, err_json("no answer from the delivery worker"), None),
    }
}

fn worker(s: Arc<Shared>, rx: Arc<Mutex<Receiver<Job>>>) {
    loop {
        let job = {
            let Ok(g) = rx.lock() else { return };
            g.recv_timeout(Duration::from_millis(50))
        };
        match job {
            Ok(job) => {
                s.queued.fetch_sub(1, Ordering::SeqCst);
                let report = if s.stop.load(Ordering::SeqCst) { s.notts.send_held(&job.msg, &job.opts, "held: server shutdown") } else { s.notts.send(&job.msg, &job.opts) };
                if let Ok(mut inf) = s.inflight.lock() {
                    if let Some(n) = inf.get_mut(&job.app) {
                        *n = n.saturating_sub(1);
                        if *n == 0 {
                            inf.remove(&job.app);
                        }
                    }
                }
                let _ = job.reply.send(report);
            }
            Err(RecvTimeoutError::Timeout) => {
                if s.stop.load(Ordering::SeqCst) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Periodic outbox flush and digest check.
fn housekeeping(s: Arc<Shared>) {
    let mut last = Instant::now();
    while !s.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(20));
        if last.elapsed() >= s.opts.flush_every {
            last = Instant::now();
            let _ = s.notts.digest(false, false);
            let _ = s.notts.flush();
        }
    }
}

/// For tests and callers that want the disposition without HTTP.
pub fn status_of(r: &Report) -> u16 {
    match (r.exit_code, r.disposition) {
        (0, _) => 200,
        (3, Disposition::Queued) | (3, _) => 202,
        _ => 502,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_is_per_app_and_refills() {
        let mut r = RateLimiter::new(60);
        for _ in 0..60 {
            assert!(r.allow("a", 0).is_ok());
        }
        assert!(r.allow("a", 0).is_err());
        assert!(r.allow("b", 0).is_ok(), "another app is not affected");
        assert!(r.allow("a", 1_000).is_ok(), "one token per second at 60/min");
        assert!(r.allow("a", 1_000).is_err());
        assert_eq!(r.allow("a", 1_000), Err(1));
    }

    #[test]
    fn rate_limiter_bounds_its_own_memory() {
        let mut r = RateLimiter::new(10);
        for i in 0..(MAX_APPS + 500) {
            let _ = r.allow(&format!("app{i}"), 0);
        }
        assert!(r.buckets.len() <= MAX_APPS + 1);
    }

    #[test]
    fn loopback_only() {
        assert!(parse_loopback("127.0.0.1:8080").is_ok());
        assert!(parse_loopback("[::1]:8080").is_ok());
        assert!(parse_loopback("localhost:80").is_ok());
        for bad in ["0.0.0.0:80", "192.168.1.5:80", "[::]:80", "example.com:80", "8080"] {
            assert!(parse_loopback(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn send_body_parsing() {
        let (m, o) = parse_send(br#"{"app":"judged","severity":"warn","title":"T","body":"B","link":"http://x","key":"k","state":"fail","to":["a"],"dry_run":true}"#, false).unwrap();
        assert_eq!((m.app.as_str(), m.severity), ("judged", Severity::Warn));
        assert!(o.dry_run && o.key.as_deref() == Some("k") && o.state == Some(Severity::Fail) && o.to == Some(vec!["a".to_string()]));
        for bad in ["[]", "nope", r#"{"title":1}"#, r#"{}"#, r#"{"title":"t","severity":"loud"}"#, r#"{"title":"t","state":"info"}"#, r#"{"title":"t","to":"a"}"#, r#"{"title":"t","dry_run":"yes"}"#] {
            assert!(parse_send(bad.as_bytes(), false).is_err(), "{bad}");
        }
    }
}
