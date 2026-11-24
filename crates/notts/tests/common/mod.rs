//! Shared test helpers: a scripted mock HTTP server, a mock SMTP server, temp dirs, secret files.
#![allow(dead_code)]

use notts::{FakeClock, Init, Notts};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static N: AtomicU64 = AtomicU64::new(0);

/// A fresh directory under $TMPDIR (the harness points it at a disk, not RAM). Never removed.
pub fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("notts-test-{}-{}-{}", std::process::id(), tag, N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn secret_file(dir: &Path, name: &str, content: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, content).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    p.to_string_lossy().into_owned()
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

#[derive(Clone, Debug)]
pub enum Resp {
    Status(u16, Vec<(String, String)>, String),
    /// read the request, then wait this long before answering 200 (timeout tests)
    Slow(Duration),
    /// read the request and close the socket without an answer
    Drop,
}

pub fn ok() -> Resp {
    Resp::Status(200, vec![], "{\"ok\":true}".into())
}

pub fn status(code: u16) -> Resp {
    Resp::Status(code, vec![], "{}".into())
}

pub struct Mock {
    pub addr: String,
    reqs: Arc<Mutex<Vec<Recorded>>>,
    script: Arc<Mutex<Vec<Resp>>>,
}

impl Mock {
    /// Answers `script` in order, then 200 forever.
    pub fn start(script: Vec<Resp>) -> Mock {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script));
        let (r2, s2) = (reqs.clone(), script.clone());
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let (r3, s3) = (r2.clone(), s2.clone());
                std::thread::spawn(move || serve_one(c, r3, s3));
            }
        });
        Mock { addr, reqs, script }
    }
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn requests(&self) -> Vec<Recorded> {
        self.reqs.lock().unwrap().clone()
    }
    pub fn count(&self) -> usize {
        self.reqs.lock().unwrap().len()
    }
    pub fn push(&self, r: Resp) {
        self.script.lock().unwrap().push(r);
    }
}

fn serve_one(c: TcpStream, reqs: Arc<Mutex<Vec<Recorded>>>, script: Arc<Mutex<Vec<Resp>>>) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(10)));
    let mut rd = BufReader::new(c.try_clone().unwrap());
    let mut c = c;
    let mut line = String::new();
    if rd.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut p = line.split_whitespace();
    let (method, path) = (p.next().unwrap_or("").to_string(), p.next().unwrap_or("").to_string());
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if rd.read_line(&mut h).unwrap_or(0) == 0 {
            return;
        }
        let h = h.trim_end().to_string();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut body = vec![0u8; len];
    if rd.read_exact(&mut body).is_err() {
        return;
    }
    reqs.lock().unwrap().push(Recorded { method, path, headers, body });
    let resp = {
        let mut s = script.lock().unwrap();
        if s.is_empty() {
            ok()
        } else {
            s.remove(0)
        }
    };
    match resp {
        Resp::Drop => {}
        Resp::Slow(d) => {
            std::thread::sleep(d);
            let _ = write_resp(&mut c, 200, &[], "{}");
        }
        Resp::Status(code, hs, b) => {
            let _ = write_resp(&mut c, code, &hs, &b);
        }
    }
}

fn write_resp(c: &mut TcpStream, code: u16, hs: &[(String, String)], body: &str) -> std::io::Result<()> {
    let mut s = format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n", body.len());
    for (k, v) in hs {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("\r\n");
    s.push_str(body);
    c.write_all(s.as_bytes())?;
    c.flush()
}

/// A bare-bones SMTP server: records the command lines and the DATA payload of every session.
pub struct Smtp {
    pub port: u16,
    pub sessions: Arc<Mutex<Vec<SmtpSession>>>,
}

#[derive(Clone, Debug, Default)]
pub struct SmtpSession {
    pub commands: Vec<String>,
    pub data: String,
}

impl Smtp {
    /// `reject_rcpt`: answer RCPT with 550.
    pub fn start(reject_rcpt: bool, offer_auth: bool) -> Smtp {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let s2 = sessions.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let s3 = s2.clone();
                std::thread::spawn(move || smtp_session(c, s3, reject_rcpt, offer_auth));
            }
        });
        Smtp { port, sessions }
    }
    pub fn sessions(&self) -> Vec<SmtpSession> {
        self.sessions.lock().unwrap().clone()
    }
}

fn smtp_session(c: TcpStream, out: Arc<Mutex<Vec<SmtpSession>>>, reject_rcpt: bool, offer_auth: bool) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(10)));
    let mut rd = BufReader::new(c.try_clone().unwrap());
    let mut w = c;
    let mut s = SmtpSession::default();
    let _ = w.write_all(b"220 mock ESMTP\r\n");
    loop {
        let mut line = String::new();
        if rd.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let cmd = line.trim_end().to_string();
        let up = cmd.to_ascii_uppercase();
        s.commands.push(cmd.clone());
        let reply: &str = if up.starts_with("EHLO") {
            if offer_auth {
                "250-mock\r\n250 AUTH PLAIN\r\n"
            } else {
                "250 mock\r\n"
            }
        } else if up.starts_with("AUTH") {
            "235 ok\r\n"
        } else if up.starts_with("RCPT") && reject_rcpt {
            "550 no such user\r\n"
        } else if up.starts_with("DATA") {
            let _ = w.write_all(b"354 go ahead\r\n");
            let mut data = String::new();
            loop {
                let mut l = String::new();
                if rd.read_line(&mut l).unwrap_or(0) == 0 || l == ".\r\n" {
                    break;
                }
                data.push_str(&l);
            }
            s.data = data;
            "250 queued\r\n"
        } else if up.starts_with("QUIT") {
            let _ = w.write_all(b"221 bye\r\n");
            break;
        } else {
            "250 ok\r\n"
        };
        let _ = w.write_all(reply.as_bytes());
    }
    out.lock().unwrap().push(s);
}

/// Build a `Notts` on a private state dir with a fake clock and no proxy.
pub fn notts(conf: &str, dir: &Path, clock: Arc<FakeClock>) -> Notts {
    let init = Init { clock, state_dir: Some(dir.join("state")), env: Arc::new(|k| std::env::var(k).ok()), env_proxy: false };
    Notts::from_str_with(conf, init).unwrap_or_else(|e| panic!("config: {e}"))
}

/// 2026-11-24 12:00:00 UTC-ish, noon local on the fake clock.
pub fn clock() -> Arc<FakeClock> {
    let c = Arc::new(FakeClock::new(1_795_600_000));
    c.set_local_minutes(12 * 60);
    c
}
