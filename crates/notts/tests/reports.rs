//! Whole reports: attachments on every channel, the split of long text, "full report" pointers,
//! and what the history may and may not contain.

mod common;
use common::*;
use notts::{Attachment, Message, SendOptions, Severity, Status};

const REPORT: &str = "# Audit 2026-11-24\n\n| finding | where |\n|---|---|\n| open port 22 | host-a |\n| zażółć gęślą jaźń | /etc/ssh |\n";

fn conf_fast(rest: &str) -> String {
    format!("set timeout=2 retries=1 backoff=10\n{rest}\n")
}

fn report_msg() -> Message {
    Message::new("Audit finished").body("2 findings, 1 new").severity(Severity::Warn).app("judged").attach(Attachment::new("audit-2026-11-24.md", REPORT.as_bytes().to_vec()))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn telegram_sends_the_summary_then_the_file_with_send_document() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("tgdoc");
    let tok = secret_file(&d, "tok", "9:TGSECRET");
    let conf = conf_fast(&format!("channel tg type=telegram token_file={tok} chat=77 api={}", mock.url()));
    let r = notts(&conf, &d, clock()).send(&report_msg(), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!((r.results[0].delivered_as, r.results[0].parts), (Some("attachment"), 1));
    let q = mock.requests();
    assert_eq!(q.len(), 2);
    assert_eq!(q[0].path, "/bot9:TGSECRET/sendMessage");
    assert!(q[0].json()["text"].as_str().unwrap().contains("2 findings, 1 new"));
    assert_eq!(q[1].path, "/bot9:TGSECRET/sendDocument");
    let ct = q[1].header("content-type").unwrap();
    assert!(ct.starts_with("multipart/form-data; boundary="), "{ct}");
    let b = &q[1].body;
    assert!(find(b, b"name=\"chat_id\"\r\n\r\n77\r\n").is_some());
    assert!(find(b, b"name=\"document\"; filename=\"audit-2026-11-24.md\"").is_some());
    assert!(find(b, b"Content-Type: text/markdown").is_some());
    assert!(find(b, REPORT.as_bytes()).is_some(), "the file bytes arrive unchanged");
    assert!(!r.to_json().contains("TGSECRET"));
}

#[test]
fn discord_sends_text_and_file_in_one_multipart_request() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("dcdoc");
    let hook = secret_file(&d, "hook", &format!("{}/api/webhooks/1/DCSECRET", mock.url()));
    let conf = conf_fast(&format!("channel dc type=discord webhook_file={hook}"));
    let r = notts(&conf, &d, clock()).send(&report_msg(), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(r.results[0].delivered_as, Some("attachment"));
    let q = mock.requests();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].path, "/api/webhooks/1/DCSECRET");
    assert!(q[0].header("content-type").unwrap().starts_with("multipart/form-data"));
    let b = &q[0].body;
    assert!(find(b, b"name=\"payload_json\"").is_some() && find(b, b"2 findings, 1 new").is_some());
    assert!(find(b, b"name=\"files[0]\"; filename=\"audit-2026-11-24.md\"").is_some());
    assert!(find(b, REPORT.as_bytes()).is_some());
}

#[test]
fn matrix_uploads_the_media_then_posts_an_m_file_event() {
    let mock = Mock::start(vec![ok(), Resp::Status(200, vec![], r#"{"content_uri":"mxc://example.org/AbCd"}"#.into())]);
    let d = tmpdir("mxdoc");
    let tok = secret_file(&d, "tok", "syt_MXSECRET");
    let conf = conf_fast(&format!("channel mx type=matrix homeserver={} room=!r:example.org token_file={tok}", mock.url()));
    let r = notts(&conf, &d, clock()).send(&report_msg().with_id("m1"), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(r.results[0].delivered_as, Some("attachment"));
    let q = mock.requests();
    assert_eq!(q.len(), 3, "text event, upload, file event");
    assert_eq!(q[0].method, "PUT");
    assert_eq!((q[1].method.as_str(), q[1].path.as_str()), ("POST", "/_matrix/media/v3/upload?filename=audit-2026-11-24.md"));
    assert_eq!(q[1].header("authorization"), Some("Bearer syt_MXSECRET"));
    assert_eq!(q[1].header("content-type"), Some("text/markdown"));
    assert_eq!(q[1].body, REPORT.as_bytes());
    assert_eq!(q[2].method, "PUT");
    assert!(q[2].path.ends_with("/send/m.room.message/notts-m1.f-mx-file"), "{}", q[2].path);
    let j = q[2].json();
    assert_eq!(
        (j["msgtype"].as_str(), j["url"].as_str(), j["body"].as_str(), j["filename"].as_str()),
        (Some("m.file"), Some("mxc://example.org/AbCd"), Some("audit-2026-11-24.md"), Some("audit-2026-11-24.md"))
    );
    assert_eq!(j["info"]["size"], REPORT.len());
}

#[test]
fn matrix_upload_without_content_uri_is_a_clear_failure() {
    let mock = Mock::start(vec![ok(), ok()]);
    let d = tmpdir("mxbad");
    let tok = secret_file(&d, "tok", "t");
    let conf = conf_fast(&format!("channel mx type=matrix homeserver={} room=!r:e token_file={tok}", mock.url()));
    let r = notts(&conf, &d, clock()).send(&report_msg(), &SendOptions::new());
    assert_eq!(r.exit_code, 1);
    assert!(r.results[0].detail.contains("content_uri"), "{}", r.results[0].detail);
}

#[test]
fn ntfy_puts_the_file_with_a_filename_header() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("ntfydoc");
    let topic = secret_file(&d, "topic", "secret-topic-77");
    let conf = conf_fast(&format!("channel phone type=ntfy server={} topic_file={topic}", mock.url()));
    let r = notts(&conf, &d, clock()).send(&report_msg(), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(r.results[0].delivered_as, Some("attachment"));
    let q = mock.requests();
    assert_eq!(q.len(), 1);
    assert_eq!((q[0].method.as_str(), q[0].path.as_str()), ("PUT", "/secret-topic-77"));
    assert_eq!(q[0].header("filename"), Some("audit-2026-11-24.md"));
    assert_eq!(q[0].header("message"), Some("2 findings, 1 new"));
    assert_eq!(q[0].header("title"), Some("Audit finished"));
    assert_eq!(q[0].header("priority"), Some("4"));
    assert_eq!(q[0].body, REPORT.as_bytes());
    assert!(!format!("{}{:?}", r.to_json(), r).contains("secret-topic"));
}

#[test]
fn ntfy_non_ascii_text_goes_in_rfc2047_headers() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("ntfyutf");
    let topic = secret_file(&d, "topic", "t77");
    let conf = conf_fast(&format!("channel phone type=ntfy server={} topic_file={topic}", mock.url()));
    let m = Message::new("Zażółć").body("gęślą").attach(Attachment::new("raport.md", b"x".to_vec()));
    assert_eq!(notts(&conf, &d, clock()).send(&m, &SendOptions::new()).exit_code, 0);
    let q = &mock.requests()[0];
    let t = q.header("title").unwrap();
    assert!(t.starts_with("=?UTF-8?B?") && t.ends_with("?="), "{t}");
    let inner = &t["=?UTF-8?B?".len()..t.len() - 2];
    assert_eq!(String::from_utf8(notts::text::base64_decode(inner).unwrap()).unwrap(), "Zażółć");
}

#[test]
fn email_carries_the_file_as_a_mime_attachment() {
    let smtp = Smtp::start(false, false);
    let d = tmpdir("maildoc");
    let conf = conf_fast(&format!("channel mail type=email host=127.0.0.1 port={} security=none from=a@b.c to=x@y.z", smtp.port));
    let r = notts(&conf, &d, clock()).send(&report_msg(), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(r.results[0].delivered_as, Some("attachment"));
    let data = &smtp.sessions()[0].data;
    assert!(data.contains("Content-Type: multipart/mixed; boundary="), "{data}");
    assert!(data.contains("Content-Disposition: attachment; filename=\"audit-2026-11-24.md\""));
    let boundary = data.split("boundary=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    let parts: Vec<&str> = data.split(&format!("--{boundary}")).collect();
    let file_part = parts.iter().find(|p| p.contains("Content-Disposition: attachment")).unwrap();
    let b64: String = file_part.split("\r\n\r\n").nth(1).unwrap().lines().collect();
    assert_eq!(String::from_utf8(notts::text::base64_decode(b64.trim()).unwrap()).unwrap(), REPORT);
    assert!(data.trim_end().ends_with(&format!("--{boundary}--")));
}

#[test]
fn webhook_gets_the_report_text_inline() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("whdoc");
    let conf = conf_fast(&format!("channel hook type=webhook url={}/in", mock.url()));
    let r = notts(&conf, &d, clock()).send(&report_msg(), &SendOptions::new());
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.results[0].delivered_as, Some("attachment"));
    let j = mock.requests()[0].json();
    let a = &j["attachments"][0];
    assert_eq!((a["name"].as_str(), a["text"].as_str(), a["size"].as_u64()), (Some("audit-2026-11-24.md"), Some(REPORT), Some(REPORT.len() as u64)));
    assert_eq!(a["sha256"].as_str().unwrap(), notts::text::sha256_hex(REPORT.as_bytes()));
}

#[test]
fn webhook_with_a_body_template_and_sms_get_summary_plus_pointer_never_a_silent_cut() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("ptr");
    let file = d.join("raport.md");
    std::fs::write(&file, REPORT).unwrap();
    let att = Attachment::from_path(file.to_str().unwrap(), Some("audit.md")).unwrap();
    let conf = conf_fast(&format!(r#"channel sms type=sms url={}/s to=+48111222333 body="{{\"msg\":\"{{text}}\"}}" segment=400"#, mock.url()));
    let r = notts(&conf, &d, clock()).send(&Message::new("Audit").body("2 findings").attach(att), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!((r.results[0].delivered_as, r.results[0].parts), (Some("link"), 1));
    let msg = mock.requests()[0].json()["msg"].as_str().unwrap().to_string();
    assert!(msg.contains("2 findings"), "{msg}");
    let canonical = std::fs::canonicalize(&file).unwrap();
    assert!(msg.contains(&format!("full report: {}", canonical.display())), "points at where the file is: {msg}");
}

#[test]
fn a_report_that_exists_only_in_memory_is_saved_for_the_pointer() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("save");
    let conf = conf_fast(&format!(r#"channel sms type=sms url={}/s to=+48111222333 body="{{\"msg\":\"{{text}}\"}}" segment=600"#, mock.url()));
    let r = notts(&conf, &d, clock()).send(&report_msg(), &SendOptions::new());
    assert_eq!(r.results[0].delivered_as, Some("link"));
    let msg = mock.requests()[0].json()["msg"].as_str().unwrap().to_string();
    let path = msg.split("full report: ").nth(1).unwrap().trim().to_string();
    assert!(path.starts_with(d.join("state").join("reports").to_str().unwrap()), "{path}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), REPORT);
    assert!(std::fs::metadata(&path).unwrap().permissions().mode_600());
}

trait Mode600 {
    fn mode_600(&self) -> bool;
}
impl Mode600 for std::fs::Permissions {
    fn mode_600(&self) -> bool {
        std::os::unix::fs::PermissionsExt::mode(self) & 0o077 == 0
    }
}

// ---- splitting -------------------------------------------------------------------------------

fn telegram_texts(mock: &Mock) -> Vec<String> {
    mock.requests().iter().map(|r| r.json()["text"].as_str().unwrap().to_string()).collect()
}

fn tg(mock: &Mock, d: &std::path::Path) -> String {
    let tok = secret_file(d, "tok", "1:T");
    conf_fast(&format!("channel tg type=telegram token_file={tok} chat=1 api={}", mock.url()))
}

#[test]
fn markdown_table_is_split_between_rows() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("table");
    let rows: String = (0..400).map(|i| format!("| finding {i:03} | /srv/data/dir{i}/file.conf | open |\n")).collect();
    let body = format!("| finding | path | state |\n|---|---|---|\n{rows}");
    let r = notts(&tg(&mock, &d), &d, clock()).send(&Message::new("Audit").body(body.clone()), &SendOptions::new());
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.results[0].delivered_as, Some("split"));
    let texts = telegram_texts(&mock);
    assert_eq!(texts.len() as u32, r.results[0].parts);
    assert!(texts.len() >= 4 && texts.len() <= 10);
    let mut seen = 0;
    for (i, t) in texts.iter().enumerate() {
        assert!(t.encode_utf16().count() <= 4096);
        assert!(t.contains(&format!("({}/{})", i + 1, texts.len())));
        for l in t.lines().filter(|l| l.starts_with("| finding ") && !l.starts_with("| finding |")) {
            assert!(l.ends_with("| open |"), "a table row is never cut: {l}");
            seen += 1;
        }
    }
    assert_eq!(seen, 400, "every row exactly once");
}

#[test]
fn a_6000_character_single_line_is_split_without_loss() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("longline");
    let line = "abcdefghij".repeat(600);
    assert_eq!(line.len(), 6000);
    let r = notts(&tg(&mock, &d), &d, clock()).send(&Message::new("t").body(line.clone()), &SendOptions::new());
    assert_eq!((r.exit_code, r.results[0].delivered_as, r.results[0].parts), (0, Some("split"), 2));
    let texts = telegram_texts(&mock);
    let joined: String = texts.iter().map(|t| t.split("\n\n").nth(1).unwrap().to_string()).collect();
    assert_eq!(joined, line);
}

#[test]
fn polish_text_is_not_cut_in_half_on_discord() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("pl");
    let hook = secret_file(&d, "hook", &format!("{}/api/webhooks/1/X", mock.url()));
    let conf = conf_fast(&format!("channel dc type=discord webhook_file={hook}"));
    let line = "zażółć gęślą jaźń ".repeat(300); // 5400 chars, one line
    let r = notts(&conf, &d, clock()).send(&Message::new("Raport").body(line.clone()), &SendOptions::new());
    assert_eq!(r.results[0].delivered_as, Some("split"));
    let mut joined = String::new();
    for q in mock.requests() {
        let c = q.json()["content"].as_str().unwrap().to_string();
        assert!(c.encode_utf16().count() <= 2000, "{}", c.encode_utf16().count());
        assert!(!c.contains('\u{fffd}'));
        joined.push_str(c.split_once('\n').unwrap().1);
    }
    assert_eq!(joined, line);
}

#[test]
fn empty_body_is_one_message() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("empty");
    let r = notts(&tg(&mock, &d), &d, clock()).send(&Message::new("just a title"), &SendOptions::new());
    assert_eq!((r.exit_code, r.results[0].delivered_as, r.results[0].parts), (0, Some("message"), 1));
    assert_eq!(mock.count(), 1);
}

#[test]
fn matrix_is_capped_with_a_visible_pointer_to_the_saved_text() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("mxcap");
    let tok = secret_file(&d, "tok", "t");
    let conf = conf_fast(&format!("channel mx type=matrix homeserver={} room=!r:e token_file={tok}", mock.url()));
    let body: String = (0..9000).map(|i| format!("line {i:05} of a very long report\n")).collect();
    let r = notts(&conf, &d, clock()).send(&Message::new("Big").body(body), &SendOptions::new());
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(r.results[0].delivered_as, Some("truncated"));
    let q = mock.requests();
    assert_eq!(q.len(), 10, "capped at 10 parts");
    let last = q[9].json()["body"].as_str().unwrap().to_string();
    assert!(last.contains("[...truncated:") && last.contains("more part(s) not sent") && last.contains("/reports/"), "{}", &last[last.len().saturating_sub(300)..]);
    let path = last.split("full text: ").nth(1).unwrap().trim_end_matches(']').trim().to_string();
    let full = std::fs::read_to_string(&path).unwrap();
    assert!(full.contains("line 08999"), "the saved copy has the whole text");
}

#[test]
fn a_failing_part_queues_that_part_and_the_rest_and_flush_sends_only_those() {
    // part 1 ok, part 2 gets 503 twice (1 try + 1 retry) then the server is fine again
    let mock = Mock::start(vec![ok(), status(503), status(503)]);
    let d = tmpdir("partial");
    let body: String = (0..300).map(|i| format!("row {i:03} {}\n", "x".repeat(60))).collect();
    let n = notts(&tg(&mock, &d), &d, clock());
    let r = n.send(&Message::new("Big").body(body), &SendOptions::new());
    assert_eq!(r.exit_code, 3, "{r:?}");
    assert_eq!(r.results[0].status, Status::Queued);
    let total = r.results[0].parts as usize;
    assert!(total >= 4);
    assert_eq!(n.outbox_len(), total - 1, "part 1 is delivered, parts 2.. wait in the outbox");
    let sent_before = mock.count();
    let f = n.flush();
    assert_eq!((f.delivered, f.remaining), (total - 1, 0), "{f:?}");
    assert_eq!(mock.count() - sent_before, total - 1, "nothing is sent twice");
    let texts = telegram_texts(&mock);
    let numbered: Vec<usize> = texts.iter().filter_map(|t| t.split('(').nth(1).and_then(|x| x.split('/').next()).and_then(|x| x.parse().ok())).collect();
    assert_eq!(numbered.iter().filter(|&&i| i == 1).count(), 1, "part 1 only once");
}

// ---- state change, history ---------------------------------------------------------------------

fn judged_state(n: &notts::Notts, state: Severity, with_file: bool) -> notts::Report {
    let mut m = Message::new("Audit").body(format!("state {}", state.as_str())).severity(state).app("judged");
    if with_file {
        m = m.attach(Attachment::new("audit-2026-11-24.md", REPORT.as_bytes().to_vec()));
    }
    n.send(&m, &SendOptions::new().key("judged-audit").state(state))
}

#[test]
fn unchanged_failing_audit_is_not_resent_and_resolved_carries_no_file() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("state");
    let tok = secret_file(&d, "tok", "1:T");
    let conf = conf_fast(&format!("channel tg type=telegram token_file={tok} chat=1 api={}", mock.url()));
    let clk = clock();
    let n = notts(&conf, &d, clk.clone());
    // ok -> fail: summary and file
    let r1 = judged_state(&n, Severity::Fail, true);
    assert_eq!((r1.exit_code, r1.disposition.as_str()), (0, "sent"));
    let paths: Vec<String> = mock.requests().iter().map(|r| r.path.rsplit('/').next().unwrap().to_string()).collect();
    assert_eq!(paths, vec!["sendMessage", "sendDocument"]);
    // the same failing audit comes back every 6 hours: nothing is sent
    for _ in 0..3 {
        clk.advance(6 * 3600);
        let r = judged_state(&n, Severity::Fail, true);
        assert_eq!(r.disposition.as_str(), "suppressed", "{r:?}");
        assert!(r.reason.contains("unchanged"), "{}", r.reason);
    }
    assert_eq!(mock.count(), 2, "no request for the unchanged reports");
    // fail -> ok: "resolved", without the file
    clk.advance(6 * 3600);
    let r = judged_state(&n, Severity::Ok, true);
    assert_eq!(r.disposition.as_str(), "sent", "{r:?}");
    let q = mock.requests();
    assert_eq!(q.len(), 3, "one more request, and it is a message, not a document");
    assert!(q[2].path.ends_with("/sendMessage"));
    // history: every decision, the suppressed ones with their reason
    let h = n.history();
    let decisions: Vec<&str> = h.iter().map(|e| e.decision.as_str()).collect();
    assert_eq!(decisions, vec!["sent", "suppressed", "suppressed", "suppressed", "sent"]);
    assert!(h[1].reason.contains("unchanged"));
    assert!(h[4].reason.contains("attachment not sent: resolved"), "{}", h[4].reason);
}

#[test]
fn history_records_name_size_and_sha256_but_never_the_contents() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("hist");
    let conf = conf_fast(&format!("channel hook type=webhook url={}/in", mock.url()));
    let n = notts(&conf, &d, clock());
    let secret_in_report = "CONTENT-MARKER-12345";
    let m = Message::new("Audit").body("s").attach(Attachment::new("a.md", format!("{secret_in_report} and more").into_bytes()));
    n.send(&m, &SendOptions::new());
    let raw = std::fs::read_to_string(d.join("state").join("history.log")).unwrap();
    assert!(!raw.contains(secret_in_report), "{raw}");
    assert!(
        raw.contains("a.md") && raw.contains(&format!("size={}", secret_in_report.len() + 9)) && raw.contains(&notts::text::sha256_hex(format!("{secret_in_report} and more").as_bytes())),
        "{raw}"
    );
    assert!(!format!("{m:?}").contains(secret_in_report), "Debug never prints attachment bytes");
}

#[test]
fn queued_attachment_survives_the_outbox_and_flush_delivers_it() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let d = tmpdir("obx");
    let bin: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let msg = Message::new("Audit").body("s").attach(Attachment::new("blob.bin", bin.clone()));
    let down = notts(&conf_fast(&format!("channel mx type=matrix homeserver=http://127.0.0.1:{port} room=!r:e token_env=NOTTS_TEST_NEVER_SET_TOKEN")), &d, clock());
    // not configured (no secret): never "sent", nothing queued
    let r = down.send(&msg, &SendOptions::new());
    assert_eq!(r.results[0].status, Status::NotConfigured);
    let msg = Message::new("Audit again").body("s").attach(Attachment::new("blob.bin", bin.clone()));

    let tokf = secret_file(&d, "tok", "t");
    let conf_down = conf_fast(&format!("channel hook type=webhook url=http://127.0.0.1:{port}/in\nchannel mx type=matrix homeserver=http://127.0.0.1:{port} room=!r:e token_file={tokf}"));
    let n = notts(&conf_down, &d, clock());
    let r = n.send(&msg, &SendOptions::new());
    assert_eq!(r.exit_code, 3, "{r:?}");
    assert!(n.outbox_len() >= 2);
    let mock = Mock::start(vec![]);
    let conf_up = conf_fast(&format!("channel hook type=webhook url={}/in", mock.url()));
    let n2 = notts(&conf_up, &d, clock());
    let f = n2.flush();
    assert!(f.delivered >= 1, "{f:?}");
    let a = &mock.requests()[0].json()["attachments"][0];
    assert_eq!(a["base64"].as_str().unwrap(), notts::text::base64(&bin), "binary content survives the outbox byte for byte");
}

#[test]
fn attachment_limits() {
    let d = tmpdir("lim");
    let big = d.join("big.bin");
    std::fs::write(&big, vec![0u8; notts::MAX_ATTACHMENT + 1]).unwrap();
    let e = Attachment::from_path(big.to_str().unwrap(), None).unwrap_err();
    assert!(e.contains("limit"), "{e}");
    assert!(Attachment::from_path(d.to_str().unwrap(), None).unwrap_err().contains("not a regular file"));
    assert!(Attachment::from_path("/nonexistent/x.md", None).is_err());
    assert_eq!(Attachment::new("../../etc/passwd", vec![]).name, "passwd");
    assert_eq!(Attachment::new("a\"b;c\nd.md", vec![]).name, "abcd.md");
    assert_eq!(Attachment::new("", vec![]).name, "attachment.txt");
}
