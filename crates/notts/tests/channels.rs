//! Every channel against a local mock: exact path, headers, body, masking, limits, retry, 429.

mod common;
use common::*;
use notts::{Message, SendOptions, Severity, Status};
use std::time::Duration;

fn conf_fast(rest: &str) -> String {
    format!("set timeout=2 retries=2 backoff=10\n{rest}\n")
}

#[test]
fn telegram_exact_request_and_escaping() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("tg");
    let tok = secret_file(&d, "tok", "123:SECRETTOKEN\n");
    let conf = conf_fast(&format!("channel tg type=telegram token_file={tok} chat=-1001234 api={}", mock.url()));
    let m = Message::new("Disk <full> & more").body("a < b & c > d\nzażółć gęślą jaźń 🙂").severity(Severity::Warn).app("judged").link("https://example.org/x?a=1&b=2");
    let n = notts(&conf, &d, clock());
    let r = n.send(&m, &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    let q = mock.requests();
    assert_eq!(q.len(), 1);
    assert_eq!((q[0].method.as_str(), q[0].path.as_str()), ("POST", "/bot123:SECRETTOKEN/sendMessage"));
    assert_eq!(q[0].header("content-type"), Some("application/json"));
    let j = q[0].json();
    assert_eq!(j["chat_id"], "-1001234");
    assert_eq!(j["parse_mode"], "HTML");
    let t = j["text"].as_str().unwrap();
    assert!(t.contains("<b>Disk &lt;full&gt; &amp; more</b>"), "{t}");
    assert!(t.contains("a &lt; b &amp; c &gt; d"), "{t}");
    assert!(t.contains("zażółć gęślą jaźń 🙂"), "unicode survives: {t}");
    assert!(t.contains("https://example.org/x?a=1&amp;b=2"), "{t}");
    let shown = format!("{r:?} {}", r.to_json());
    assert!(!shown.contains("SECRETTOKEN"), "token leaked: {shown}");
}

#[test]
fn telegram_long_body_is_split_at_4096_never_inside_an_entity() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("tglim");
    let tok = secret_file(&d, "tok", "1:T");
    let conf = conf_fast(&format!("channel tg type=telegram token_file={tok} chat=1 api={}", mock.url()));
    // every '<' becomes "&lt;" (4 units): a naive cut on the raw text would overflow or split an entity
    let body = "<".repeat(5000);
    let r = notts(&conf, &d, clock()).send(&Message::new("t").body(body), &SendOptions::new());
    assert_eq!(r.exit_code, 0);
    assert_eq!((r.results[0].delivered_as, r.results[0].parts), (Some("split"), 5));
    let q = mock.requests();
    assert_eq!(q.len(), 5);
    let mut lts = 0;
    for (i, req) in q.iter().enumerate() {
        let t = req.json()["text"].as_str().unwrap().to_string();
        assert!(t.encode_utf16().count() <= 4096, "part {i}: {}", t.encode_utf16().count());
        assert!(t.contains(&format!("({}/5)", i + 1)), "numbered in order: {t}");
        assert!(!t.contains("[...truncated]"));
        lts += t.matches("&lt;").count();
        assert_eq!(t.matches('&').count(), t.matches("&lt;").count() + t.matches("&amp;").count(), "no half entity: {t}");
    }
    assert_eq!(lts, 5000, "every character arrived exactly once");
}

#[test]
fn discord_strips_mentions_and_splits_at_2000() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("dc");
    let hook = secret_file(&d, "hook", &format!("{}/api/webhooks/42/WEBHOOKSECRET\n", mock.url()));
    let conf = conf_fast(&format!("channel dc type=discord webhook_file={hook} username=notts-bot"));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("@everyone alert").body(format!("ping @here and @Everyone {}", "x".repeat(3000))).severity(Severity::Fail), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    let q = &mock.requests()[0];
    assert_eq!((q.method.as_str(), q.path.as_str()), ("POST", "/api/webhooks/42/WEBHOOKSECRET"));
    let j = q.json();
    let c = j["content"].as_str().unwrap();
    assert!(!c.contains("@everyone") && !c.contains("@here") && !c.to_lowercase().contains("@everyone"), "{c}");
    assert!(c.contains("@\u{200b}everyone") && c.contains("@\u{200b}here"));
    assert!(c.encode_utf16().count() <= 2000);
    assert_eq!(mock.count(), 2, "3000 chars do not fit one 2000-char message: split, not cut");
    assert!(mock.requests()[1].json()["content"].as_str().unwrap().contains("(2/2)"));
    assert_eq!(j["allowed_mentions"]["parse"], serde_json::json!([]));
    assert_eq!(j["username"], "notts-bot");
    assert!(!r.to_json().contains("WEBHOOKSECRET"));
}

#[test]
fn matrix_put_with_txn_id_and_escaped_html() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("mx");
    let tok = secret_file(&d, "tok", "syt_SECRETMATRIX");
    let conf = conf_fast(&format!("channel mx type=matrix homeserver={} room=!abc:example.org token_file={tok}", mock.url()));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("<script>alert(1)</script>").body("line1\nline2 & <b>x</b>").with_id("id77"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    let q = &mock.requests()[0];
    assert_eq!(q.method, "PUT");
    assert_eq!(q.path, "/_matrix/client/v3/rooms/%21abc%3Aexample.org/send/m.room.message/notts-id77-mx");
    assert_eq!(q.header("authorization"), Some("Bearer syt_SECRETMATRIX"));
    let j = q.json();
    assert_eq!(j["msgtype"], "m.text");
    assert_eq!(j["format"], "org.matrix.custom.html");
    let h = j["formatted_body"].as_str().unwrap();
    assert!(!h.contains("<script>") && h.contains("&lt;script&gt;"), "{h}");
    assert!(h.contains("line1<br/>line2 &amp; &lt;b&gt;x&lt;/b&gt;"), "{h}");
    assert!(!r.to_json().contains("SECRETMATRIX"));
}

#[test]
fn ntfy_json_publish_with_priority_and_click() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("ntfy");
    let topic = secret_file(&d, "topic", "my-secret-topic-9f3\n");
    let tok = secret_file(&d, "tok", "tk_NTFYTOKEN");
    let conf = conf_fast(&format!("channel phone type=ntfy server={} topic_file={topic} token_file={tok}", mock.url()));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("Backup failed").body("exit 2 na hoście ąę").severity(Severity::Fail).link("https://ex.org/log"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    let q = &mock.requests()[0];
    assert_eq!((q.method.as_str(), q.path.as_str()), ("POST", "/"));
    assert_eq!(q.header("authorization"), Some("Bearer tk_NTFYTOKEN"));
    let j = q.json();
    assert_eq!(j["topic"], "my-secret-topic-9f3");
    assert_eq!(j["priority"], 5);
    assert_eq!(j["title"], "Backup failed");
    assert_eq!(j["message"], "exit 2 na hoście ąę");
    assert_eq!(j["click"], "https://ex.org/log");
    let out = format!("{}{:?}", r.to_json(), r);
    assert!(!out.contains("my-secret-topic") && !out.contains("NTFYTOKEN"), "{out}");
}

#[test]
fn webhook_template_escapes_per_content_type() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("wh");
    let conf = conf_fast(&format!(r#"channel hook type=webhook url={}/in method=POST body="{{\"t\":\"{{title}}\",\"b\":\"{{body}}\",\"s\":\"{{severity}}\",\"a\":\"{{app}}\"}}""#, mock.url()));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("say \"hi\"").body("a\\b\nc").severity(Severity::Ok).app("x"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    let q = &mock.requests()[0];
    assert_eq!((q.method.as_str(), q.path.as_str()), ("POST", "/in"));
    let j = q.json();
    assert_eq!(j["t"], "say \"hi\"", "json-escaped, round-trips: {}", q.text());
    assert_eq!(j["b"], "a\\b\nc");
    assert_eq!((j["s"].as_str(), j["a"].as_str()), (Some("ok"), Some("x")));
}

#[test]
fn webhook_default_body_and_bearer() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("wh2");
    let b = secret_file(&d, "b", "BEARERSECRET");
    let conf = conf_fast(&format!("channel hook type=webhook url={}/x bearer_file={b}", mock.url()));
    let r = notts(&conf, &d, clock()).send(&Message::new("T").body("B").app("judged"), &SendOptions::new());
    assert_eq!(r.exit_code, 0);
    let q = &mock.requests()[0];
    assert_eq!(q.header("authorization"), Some("Bearer BEARERSECRET"));
    let j = q.json();
    assert_eq!((j["title"].as_str(), j["body"].as_str(), j["app"].as_str(), j["severity"].as_str()), (Some("T"), Some("B"), Some("judged"), Some("info")));
}

#[test]
fn sms_gateway_template_segments_capped_and_phone_number_masked() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("sms");
    let to = secret_file(&d, "to", "+48123456789\n");
    let key = secret_file(&d, "key", "GATEWAYKEY");
    let conf = conf_fast(&format!(r#"channel sms type=sms url={}/send to_file={to} bearer_file={key} segment=40 segments=3 body="{{\"to\":\"{{to}}\",\"msg\":\"{{text}}\"}}""#, mock.url()));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("Disk").body("full ".repeat(30)).severity(Severity::Fail), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!((r.results[0].delivered_as, r.results[0].parts), (Some("truncated"), 3), "capped at 3 segments, said so");
    let q = mock.requests();
    assert_eq!(q.len(), 3);
    assert_eq!(q[0].path, "/send");
    assert_eq!(q[0].header("authorization"), Some("Bearer GATEWAYKEY"));
    for (i, req) in q.iter().enumerate() {
        let j = req.json();
        assert_eq!(j["to"], "+48123456789");
        let msg = j["msg"].as_str().unwrap();
        assert!(msg.chars().count() <= 40, "{msg}");
        assert!(msg.starts_with(&format!("Disk ({}/3): ", i + 1)), "{msg}");
    }
    assert!(q[2].json()["msg"].as_str().unwrap().contains("/reports/") || q[2].json()["msg"].as_str().unwrap().contains("truncated"), "the last segment points at the saved text");
    let out = format!("{}{:?}", r.to_json(), r);
    assert!(!out.contains("48123456789") && !out.contains("GATEWAYKEY"), "{out}");
}

#[test]
fn email_smtp_envelope_headers_and_utf8() {
    let smtp = Smtp::start(false, true);
    let d = tmpdir("mail");
    let pass = secret_file(&d, "pass", "SMTPPASS");
    let conf = conf_fast(&format!("channel mail type=email host=127.0.0.1 port={} security=none from=notts@example.org to=me@example.org,you@example.org user=bob pass_file={pass}", smtp.port));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("Zażółć gęślą jaźń").body("body ąę\nsecond").severity(Severity::Warn).app("judged"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    let s = &smtp.sessions()[0];
    let cmds = s.commands.join("|");
    assert!(cmds.contains("MAIL FROM:<notts@example.org>"), "{cmds}");
    assert!(cmds.contains("RCPT TO:<me@example.org>") && cmds.contains("RCPT TO:<you@example.org>"), "{cmds}");
    assert!(s.commands.iter().any(|c| c.starts_with("AUTH PLAIN")), "{cmds}");
    assert!(s.data.contains("Subject: =?UTF-8?B?"), "{}", s.data);
    assert!(s.data.contains("Content-Type: text/plain; charset=utf-8"));
    assert!(s.data.contains("X-Notts-App: judged"));
    let body_b64: String = s.data.split("\r\n\r\n").nth(1).unwrap().lines().collect();
    let decoded = notts::text::base64_decode(&body_b64).unwrap();
    assert_eq!(String::from_utf8(decoded).unwrap().replace("\r\n", "\n").trim_end(), "body ąę\nsecond");
    assert!(!r.to_json().contains("SMTPPASS"));
}

#[test]
fn email_rejected_recipient_is_a_permanent_failure_without_retry() {
    let smtp = Smtp::start(true, false);
    let d = tmpdir("mail2");
    let conf = conf_fast(&format!("channel mail type=email host=127.0.0.1 port={} security=none from=a@b.c to=x@y.z", smtp.port));
    let r = notts(&conf, &d, clock()).send(&Message::new("t").body("b"), &SendOptions::new());
    assert_eq!(r.exit_code, 1, "{r:?}");
    assert_eq!(r.results[0].status, Status::Failed);
    assert_eq!(smtp.sessions().len(), 1, "a 550 is not retried");
}

#[test]
fn desktop_sink_runs_the_command_without_a_shell() {
    let d = tmpdir("desk");
    let out = d.join("args.txt");
    let script = d.join("fake-notify");
    std::fs::write(&script, format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\n", out.display())).unwrap();
    std::os::unix::fs::PermissionsExt::set_mode(&mut std::fs::metadata(&script).unwrap().permissions(), 0o755);
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let conf = conf_fast(&format!("channel desk type=desktop cmd={}", script.display()));
    let r = notts(&conf, &d, clock()).send(&Message::new("$(touch /nonexistent/pwn) `x`").body("b; rm -rf /").severity(Severity::Fail), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    let a = std::fs::read_to_string(&out).unwrap();
    assert!(a.starts_with("-a\nnotts\n-u\ncritical\n--\n"), "{a}");
    assert!(a.contains("$(touch /nonexistent/pwn) `x`") && a.contains("b; rm -rf /"), "passed as plain arguments: {a}");
}

#[test]
fn retry_with_backoff_on_5xx_then_success() {
    let mock = Mock::start(vec![status(503), status(500)]);
    let d = tmpdir("retry");
    let conf = format!("set timeout=2 retries=2 backoff=1000\nchannel h type=webhook url={}/x", mock.url());
    let clk = clock();
    let r = notts(&conf, &d, clk.clone()).send(&Message::new("t"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(mock.count(), 3);
    let slept = clk.slept();
    assert_eq!(slept.len(), 2, "{slept:?}");
    assert!(slept[1] > slept[0], "backoff grows: {slept:?}");
    assert!(slept[0] >= Duration::from_millis(500) && slept[0] <= Duration::from_secs(2), "{slept:?}");
}

#[test]
fn retries_are_bounded_and_failure_goes_to_the_outbox() {
    let mock = Mock::start(vec![status(500), status(500), status(500), status(500), status(500)]);
    let d = tmpdir("bounded");
    let conf = format!("set timeout=2 retries=2 backoff=10\nchannel h type=webhook url={}/x", mock.url());
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("t"), &SendOptions::new());
    assert_eq!(mock.count(), 3, "1 try + 2 retries, no more");
    assert_eq!(r.exit_code, 3, "{r:?}");
    assert_eq!(r.results[0].status, Status::Queued);
    assert_eq!(n.outbox_len(), 1);
}

#[test]
fn permanent_4xx_is_not_retried_and_not_queued() {
    let mock = Mock::start(vec![status(401)]);
    let d = tmpdir("perm");
    let conf = conf_fast(&format!("channel h type=webhook url={}/x", mock.url()));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("t"), &SendOptions::new());
    assert_eq!(mock.count(), 1);
    assert_eq!(r.exit_code, 1);
    assert_eq!(r.results[0].status, Status::Failed);
    assert!(r.results[0].detail.contains("401"), "{}", r.results[0].detail);
    assert_eq!(n.outbox_len(), 0);
}

#[test]
fn http_429_honours_retry_after_header() {
    let mock = Mock::start(vec![Resp::Status(429, vec![("Retry-After".into(), "3".into())], "{}".into())]);
    let d = tmpdir("429");
    let conf = format!("set timeout=2 retries=1 backoff=10\nchannel h type=webhook url={}/x", mock.url());
    let clk = clock();
    let r = notts(&conf, &d, clk.clone()).send(&Message::new("t"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(mock.count(), 2);
    assert_eq!(clk.slept(), vec![Duration::from_secs(3)]);
}

#[test]
fn telegram_429_reads_retry_after_from_the_body() {
    let body = r#"{"ok":false,"error_code":429,"parameters":{"retry_after":2}}"#;
    let mock = Mock::start(vec![Resp::Status(429, vec![], body.into())]);
    let d = tmpdir("tg429");
    let tok = secret_file(&d, "tok", "1:T");
    let conf = format!("set timeout=2 retries=1 backoff=10\nchannel tg type=telegram token_file={tok} chat=1 api={}", mock.url());
    let clk = clock();
    let r = notts(&conf, &d, clk.clone()).send(&Message::new("t"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(clk.slept(), vec![Duration::from_secs(2)]);
}

#[test]
fn huge_retry_after_is_capped() {
    let mock = Mock::start(vec![Resp::Status(429, vec![("Retry-After".into(), "86400".into())], "{}".into())]);
    let d = tmpdir("cap");
    let conf = format!("set timeout=2 retries=1 backoff=10\nchannel h type=webhook url={}/x", mock.url());
    let clk = clock();
    let r = notts(&conf, &d, clk.clone()).send(&Message::new("t"), &SendOptions::new());
    assert!(clk.slept().iter().all(|d| *d <= Duration::from_secs(60)), "{:?}", clk.slept());
    assert_eq!(r.exit_code, 3, "a server asking for a day is not waited for: it goes to the outbox");
}

#[test]
fn timeout_is_per_channel_and_does_not_block_the_others() {
    let slow = Mock::start(vec![Resp::Slow(Duration::from_secs(5)), Resp::Slow(Duration::from_secs(5))]);
    let fast = Mock::start(vec![]);
    let d = tmpdir("tmo");
    let conf = format!("set timeout=1 retries=1 backoff=10\nchannel slow type=webhook url={}/x\nchannel fast type=webhook url={}/y", slow.url(), fast.url());
    let t0 = std::time::Instant::now();
    let r = notts(&conf, &d, clock()).send(&Message::new("t"), &SendOptions::new());
    assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
    let by = |n: &str| r.results.iter().find(|c| c.name == n).unwrap().status;
    assert_eq!(by("fast"), Status::Sent);
    assert_eq!(by("slow"), Status::Queued);
    assert_eq!(r.exit_code, 0, "one channel delivered");
    assert_eq!(fast.count(), 1);
}

#[test]
fn connection_refused_is_queued_without_leaking_the_url() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let d = tmpdir("refused");
    let hook = secret_file(&d, "u", &format!("http://127.0.0.1:{port}/hook/URLSECRET"));
    let conf = conf_fast(&format!("channel h type=webhook url_file={hook}"));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("t"), &SendOptions::new());
    assert_eq!(r.exit_code, 3, "{r:?}");
    let out = format!("{}{:?}", r.to_json(), r);
    assert!(!out.contains("URLSECRET"), "{out}");
}

#[test]
fn dropped_connection_is_retryable() {
    let mock = Mock::start(vec![Resp::Drop, ok()]);
    let d = tmpdir("drop");
    let conf = conf_fast(&format!("channel h type=webhook url={}/x", mock.url()));
    let r = notts(&conf, &d, clock()).send(&Message::new("t"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(mock.count(), 2);
}

#[test]
fn invalid_utf8_empty_and_one_megabyte_bodies() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("bodies");
    let conf = conf_fast(&format!("channel h type=webhook url={}/x", mock.url()));
    let n = notts(&conf, &d, clock());
    let bad = Message::new("t").body_bytes(&[b'o', b'k', 0xff, 0xfe, b'!']);
    assert_eq!(n.send(&bad, &SendOptions::new().no_group(true)).exit_code, 0);
    assert!(mock.requests()[0].json()["body"].as_str().unwrap().starts_with("ok"), "lossy, valid JSON");
    let empty = Message::new("only a title");
    assert_eq!(n.send(&empty, &SendOptions::new().no_group(true)).exit_code, 0);
    let big = Message::new("big").body("é".repeat(512 * 1024));
    let r = n.send(&big, &SendOptions::new().no_group(true));
    assert_eq!(r.exit_code, 0, "{r:?}");
    let sent = mock.requests()[2].json()["body"].as_str().unwrap().to_string();
    assert_eq!(sent.chars().count(), 512 * 1024, "a 1 MB body travels whole on a channel without a limit");
    let huge = Message::new("huge").body("é".repeat(2_500_000));
    assert_eq!(n.send(&huge, &SendOptions::new().no_group(true)).exit_code, 0);
    let sent = mock.requests()[3].json()["body"].as_str().unwrap().to_string();
    assert!(sent.ends_with("[...truncated]"), "beyond 2M characters the cut is visible");
}

#[test]
fn one_failing_channel_never_blocks_the_others() {
    let bad = Mock::start(vec![status(401)]);
    let good = Mock::start(vec![]);
    let d = tmpdir("iso");
    let conf = conf_fast(&format!("channel a type=webhook url={}/x\nchannel b type=webhook url={}/y\nchannel c type=webhook url={}/z", bad.url(), good.url(), good.url()));
    let r = notts(&conf, &d, clock()).send(&Message::new("t"), &SendOptions::new());
    let st: Vec<_> = r.results.iter().map(|c| (c.name.as_str(), c.status)).collect();
    assert_eq!(st, vec![("a", Status::Failed), ("b", Status::Sent), ("c", Status::Sent)]);
    assert_eq!(r.exit_code, 0);
}

#[test]
fn dry_run_contacts_nobody() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("dry");
    let conf = conf_fast(&format!("channel a type=webhook url={}/x", mock.url()));
    let n = notts(&conf, &d, clock());
    let r = n.send(&Message::new("t"), &SendOptions::new().dry_run(true));
    assert_eq!(mock.count(), 0);
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.results[0].status, Status::Skipped);
    assert!(r.results[0].detail.contains("would send via POST"), "dry-run shows what it would do: {}", r.results[0].detail);
    assert_eq!(n.outbox_len(), 0);
}
