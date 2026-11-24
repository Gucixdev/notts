//! `notts serve` against a running server: unix socket and loopback TCP, concurrency, back-pressure.

mod common;
use common::*;
use notts::secret::Secret;
use notts::serve::{ServeOpts, Server};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

struct Reply {
    code: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
    fn header(&self, k: &str) -> Option<String> {
        self.headers.lines().find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim().to_string()))
    }
}

fn parse_reply(raw: Vec<u8>) -> Reply {
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let code = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Reply { code, headers: head.to_string(), body: body.to_string() }
}

fn request(method: &str, path: &str, extra: &str, body: &str) -> String {
    format!("{method} {path} HTTP/1.1\r\nHost: notts\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn over_unix(sock: &Path, method: &str, path: &str, body: &str) -> Reply {
    let mut c = UnixStream::connect(sock).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    c.write_all(request(method, path, "", body).as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = c.read_to_end(&mut raw);
    parse_reply(raw)
}

fn over_tcp(addr: std::net::SocketAddr, method: &str, path: &str, token: Option<&str>, body: &str) -> Reply {
    let mut c = std::net::TcpStream::connect(addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    c.write_all(request(method, path, &auth, body).as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = c.read_to_end(&mut raw);
    parse_reply(raw)
}

struct Running {
    server: Server,
    n: Arc<notts::Notts>,
    sock: PathBuf,
    dir: PathBuf,
}

fn start(conf: &str, tweak: impl FnOnce(&mut ServeOpts)) -> Running {
    let dir = tmpdir("serve");
    let n = Arc::new(notts(conf, &dir, clock()));
    let sock = dir.join("notts.sock");
    let mut o = ServeOpts { socket: Some(sock.clone()), ..ServeOpts::default() };
    tweak(&mut o);
    let server = Server::start(n.clone(), o, Arc::new(AtomicBool::new(false))).unwrap_or_else(|e| panic!("start: {e}"));
    Running { server, n, sock, dir }
}

fn conf(mock: &Mock) -> String {
    format!("set timeout=5 retries=0 backoff=10\nchannel hook type=webhook url={}/in\n", mock.url())
}

#[test]
fn socket_is_mode_600_and_send_health_outbox_work() {
    let mock = Mock::start(vec![]);
    let r = start(&conf(&mock), |_| {});
    let mode = std::fs::metadata(&r.sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    let h = over_unix(&r.sock, "GET", "/v1/health", "");
    assert_eq!(h.code, 200);
    assert_eq!(h.json()["status"], "ok");
    assert_eq!(h.json()["notts_version"], notts::VERSION);
    let s = over_unix(&r.sock, "POST", "/v1/send", r#"{"app":"judged","severity":"warn","title":"Zażółć","body":"gęślą jaźń","link":"https://example.org/x"}"#);
    assert_eq!(s.code, 200, "{}", s.body);
    let j = s.json();
    assert_eq!(j["disposition"], "sent");
    assert_eq!(j["results"][0]["name"], "hook");
    assert_eq!(j["results"][0]["status"], "sent");
    assert_eq!(j["results"][0]["delivered_as"], "message");
    assert_eq!(mock.requests()[0].json()["title"], "Zażółć");
    let o = over_unix(&r.sock, "GET", "/v1/outbox", "");
    assert_eq!((o.code, o.json()["count"].as_u64()), (200, Some(0)));
    assert_eq!(over_unix(&r.sock, "GET", "/v1/nope", "").code, 404);
    assert_eq!(over_unix(&r.sock, "GET", "/v1/send", "").code, 405);
    assert_eq!(over_unix(&r.sock, "POST", "/v1/send", "{not json").code, 400);
    assert_eq!(over_unix(&r.sock, "POST", "/v1/send", r#"{"app":"x"}"#).code, 400, "a title is required");
    let sock = r.sock.clone();
    r.server.shutdown();
    assert!(!sock.exists(), "the socket is removed on shutdown");
}

#[test]
fn a_down_channel_answers_202_and_the_outbox_count_shows_it() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let r = start(&format!("set timeout=1 retries=0\nchannel hook type=webhook url=http://127.0.0.1:{port}/in\n"), |_| {});
    let s = over_unix(&r.sock, "POST", "/v1/send", r#"{"title":"t"}"#);
    assert_eq!(s.code, 202, "{}", s.body);
    assert_eq!(s.json()["exit_code"], 3);
    assert_eq!(over_unix(&r.sock, "GET", "/v1/outbox", "").json()["count"], 1);
    r.server.shutdown();
}

#[test]
fn a_failing_channel_answers_502() {
    let mock = Mock::start(vec![status(401)]);
    let r = start(&conf(&mock), |_| {});
    let s = over_unix(&r.sock, "POST", "/v1/send", r#"{"title":"t"}"#);
    assert_eq!((s.code, s.json()["exit_code"].as_i64()), (502, Some(1)), "{}", s.body);
    r.server.shutdown();
}

#[test]
fn tcp_needs_the_bearer_token_and_only_binds_loopback() {
    let mock = Mock::start(vec![]);
    let dir = tmpdir("tcp");
    let n = Arc::new(notts(&conf(&mock), &dir, clock()));
    let token = Secret::new("tok-SECRET-123");
    let opts = |listen: &str, token: Option<Secret>| ServeOpts { socket: None, listen: Some(listen.into()), token, ..ServeOpts::default() };
    let stop = || Arc::new(AtomicBool::new(false));
    for bad in ["0.0.0.0:0", "192.168.1.5:9000", "[::]:0", "10.0.0.1:1"] {
        let e = Server::start(n.clone(), opts(bad, Some(token.clone())), stop()).err().unwrap_or_else(|| panic!("{bad} must be refused"));
        assert!(e.contains("only binds loopback"), "{e}");
    }
    let e = Server::start(n.clone(), opts("127.0.0.1:0", None), stop()).err().unwrap();
    assert!(e.contains("--token-file"), "{e}");
    assert!(Server::start(n.clone(), ServeOpts::default(), stop()).err().unwrap().contains("nothing to listen on"));

    let srv = Server::start(n.clone(), opts("127.0.0.1:0", Some(token.clone())), stop()).unwrap();
    let a = srv.tcp_addr().unwrap();
    let body = r#"{"title":"hello"}"#;
    assert_eq!(over_tcp(a, "POST", "/v1/send", None, body).code, 401, "no token");
    let bad = over_tcp(a, "POST", "/v1/send", Some("wrong"), body);
    assert_eq!(bad.code, 401);
    assert!(bad.header("www-authenticate").is_some());
    assert_eq!(over_tcp(a, "GET", "/v1/health", Some("tok-SECRET-12"), "").code, 401, "a prefix of the token is wrong");
    assert_eq!(over_tcp(a, "GET", "/v1/health", Some("tok-SECRET-1234"), "").code, 401);
    assert_eq!(mock.count(), 0, "nothing was sent without a valid token");
    let ok = over_tcp(a, "POST", "/v1/send", Some("tok-SECRET-123"), body);
    assert_eq!(ok.code, 200, "{}", ok.body);
    assert!(!ok.body.contains("tok-SECRET") && !bad.body.contains("tok-SECRET"));
    assert_eq!(mock.count(), 1);
    srv.shutdown();
}

#[test]
fn oversized_body_is_413_and_nothing_is_sent() {
    let mock = Mock::start(vec![]);
    let r = start(&conf(&mock), |o| o.max_body = 2048);
    let big = format!(r#"{{"title":"t","body":"{}"}}"#, "x".repeat(5000));
    assert_eq!(over_unix(&r.sock, "POST", "/v1/send", &big).code, 413);
    assert_eq!(mock.count(), 0);
    let small = over_unix(&r.sock, "POST", "/v1/send", r#"{"title":"t"}"#);
    assert_eq!(small.code, 200, "the server is fine afterwards");
    // no Content-Length on a POST
    let mut c = UnixStream::connect(&r.sock).unwrap();
    c.write_all(b"POST /v1/send HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut raw = Vec::new();
    let _ = c.read_to_end(&mut raw);
    assert_eq!(parse_reply(raw).code, 411);
    r.server.shutdown();
}

#[test]
fn many_concurrent_clients_all_get_an_answer() {
    let mock = Mock::start(vec![]);
    let r = start(&conf(&mock), |o| {
        o.queue = 64;
        o.workers = 4;
        o.rate_per_min = 0;
    });
    let sock = r.sock.clone();
    let hs: Vec<_> = (0..24)
        .map(|i| {
            let sock = sock.clone();
            std::thread::spawn(move || over_unix(&sock, "POST", "/v1/send", &format!(r#"{{"app":"svc{}","title":"event {i}","body":"zażółć {i}"}}"#, i % 6)).code)
        })
        .collect();
    let codes: Vec<u16> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(codes.iter().all(|c| *c == 200), "{codes:?}");
    assert_eq!(mock.count(), 24);
    let mut seen: Vec<String> = mock.requests().iter().map(|q| q.json()["title"].as_str().unwrap().to_string()).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 24, "every message exactly once");
    r.server.shutdown();
}

#[test]
fn a_full_queue_answers_429_with_retry_after_instead_of_hanging() {
    let slow = Mock::start(vec![Resp::Slow(Duration::from_millis(700)); 40]);
    let r = start(&conf(&slow), |o| {
        o.queue = 2;
        o.workers = 1;
        o.rate_per_min = 0;
    });
    let sock = r.sock.clone();
    let hs: Vec<_> = (0..12)
        .map(|i| {
            let sock = sock.clone();
            std::thread::spawn(move || {
                let rep = over_unix(&sock, "POST", "/v1/send", &format!(r#"{{"app":"a{i}","title":"t{i}"}}"#));
                (rep.code, rep.header("retry-after"))
            })
        })
        .collect();
    let res: Vec<(u16, Option<String>)> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    let ok = res.iter().filter(|r| r.0 == 200).count();
    let full: Vec<_> = res.iter().filter(|r| r.0 == 429).collect();
    assert!(ok >= 1 && !full.is_empty() && ok + full.len() == 12, "{res:?}");
    assert!(full.iter().all(|r| r.1.as_deref().and_then(|v| v.parse::<u64>().ok()).is_some()), "429 carries Retry-After: {res:?}");
    assert_eq!(slow.count(), ok, "what was refused was not sent");
    r.server.shutdown();
}

#[test]
fn per_app_rate_limit_does_not_affect_other_apps() {
    let mock = Mock::start(vec![]);
    let r = start(&conf(&mock), |o| o.rate_per_min = 3);
    let send = |app: &str, i: u32| over_unix(&r.sock, "POST", "/v1/send", &format!(r#"{{"app":"{app}","title":"t{i}"}}"#));
    let codes: Vec<u16> = (0..5).map(|i| send("noisy", i).code).collect();
    assert_eq!(codes, vec![200, 200, 200, 429, 429]);
    let limited = send("noisy", 9);
    assert!(limited.header("retry-after").is_some());
    assert_eq!(send("calm", 0).code, 200, "one noisy service never pushes the others out");
    r.server.shutdown();
}

#[test]
fn attachments_by_base64_anywhere_and_by_path_only_on_the_unix_socket() {
    let mock = Mock::start(vec![]);
    let dir = tmpdir("servatt");
    let n = Arc::new(notts(&conf(&mock), &dir, clock()));
    let sock = dir.join("s.sock");
    let token = Secret::new("t0k");
    let srv = Server::start(
        n.clone(),
        ServeOpts { socket: Some(sock.clone()), listen: Some("127.0.0.1:0".into()), token: Some(token), rate_per_min: 0, ..ServeOpts::default() },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let a = srv.tcp_addr().unwrap();
    let file = dir.join("raport.md");
    std::fs::write(&file, "# raport\n").unwrap();
    let b64 = notts::text::base64("# inline\n".as_bytes());
    let by_b64 = format!(r#"{{"app":"judged","title":"a","body":"s","attachments":[{{"name":"r.md","base64":"{b64}"}}]}}"#);
    let by_path = format!(r#"{{"app":"judged","title":"b","body":"s","attachments":[{{"path":"{}","name":"p.md"}}]}}"#, file.display());
    assert_eq!(over_tcp(a, "POST", "/v1/send", Some("t0k"), &by_b64).code, 200);
    let denied = over_tcp(a, "POST", "/v1/send", Some("t0k"), &by_path);
    assert_eq!(denied.code, 400, "{}", denied.body);
    assert!(denied.json()["error"].as_str().unwrap().contains("unix socket"));
    let allowed = over_unix(&sock, "POST", "/v1/send", &by_path);
    assert_eq!(allowed.code, 200, "{}", allowed.body);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].json()["attachments"][0]["text"], "# inline\n");
    assert_eq!(reqs[1].json()["attachments"][0]["name"], "p.md");
    assert_eq!(reqs[1].json()["attachments"][0]["text"], "# raport\n");
    for bad in [
        r#"{"title":"t","attachments":[{"name":"x"}]}"#.to_string(),
        r#"{"title":"t","attachments":[{"base64":"QQ=="}]}"#.to_string(),
        r#"{"title":"t","attachments":[{"name":"x","base64":"!!!"}]}"#.to_string(),
        r#"{"title":"t","attachments":"nope"}"#.to_string(),
        format!(r#"{{"title":"t","attachments":[{}]}}"#, [r#"{"name":"x","base64":"QQ=="}"#; 4].join(",")),
    ] {
        assert_eq!(over_unix(&sock, "POST", "/v1/send", &bad).code, 400, "{bad}");
    }
    srv.shutdown();
}

#[test]
fn shutdown_parks_queued_messages_in_the_outbox() {
    let slow = Mock::start(vec![Resp::Slow(Duration::from_millis(800)); 20]);
    let r = start(&conf(&slow), |o| {
        o.queue = 8;
        o.workers = 1;
        o.rate_per_min = 0;
    });
    let sock = r.sock.clone();
    let hs: Vec<_> = (0..4)
        .map(|i| {
            let sock = sock.clone();
            std::thread::spawn(move || over_unix(&sock, "POST", "/v1/send", &format!(r#"{{"app":"a{i}","title":"t{i}"}}"#)).code)
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let n = r.n.clone();
    r.server.shutdown();
    let codes: Vec<u16> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(codes.iter().all(|c| *c == 200 || *c == 202), "every caller got a clean answer: {codes:?}");
    let delivered = slow.count();
    assert_eq!(delivered + n.outbox_len(), 4, "each message is either delivered or waiting in the outbox: {codes:?}");
    assert!(n.outbox_len() >= 1, "work that was still queued is kept, not dropped: {codes:?}");
    let _ = &r.dir;
}
