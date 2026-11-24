//! Flood control against a mock server with a fake clock: grouping, state change, digest, flood guard,
//! per-channel policy, quiet hours, a damaged state file, and two processes sharing one state directory.

mod common;
use common::*;
use notts::outbox::{Entry, Outbox};
use notts::{Message, SendOptions, Severity, Status};

fn conf(mock: &Mock, extra: &str) -> String {
    format!("set timeout=2 retries=0 backoff=10\nchannel hook type=webhook url={}/in\n{extra}\n", mock.url())
}

fn titles(mock: &Mock) -> Vec<String> {
    mock.requests().iter().map(|r| r.json()["title"].as_str().unwrap_or("").to_string()).collect()
}

#[test]
fn five_hundred_messages_from_five_apps_give_a_small_bounded_number_of_sends() {
    let mock = Mock::start(vec![]);
    let d = tmpdir_ram("five");
    let clk = clock();
    let n = notts(&conf(&mock, "set group=10m"), &d, clk.clone());
    let (mut sent, mut grouped) = (0, 0);
    for i in 0..500u64 {
        let app = format!("app{}", i % 5);
        let m = Message::new(format!("job {}", (i / 5) % 4)).app(app).severity(Severity::Warn);
        match n.send(&m, &SendOptions::new()).disposition.as_str() {
            "sent" => sent += 1,
            "grouped" => grouped += 1,
            other => panic!("unexpected {other}"),
        }
        clk.advance(1);
    }
    assert_eq!((sent, grouped), (20, 480), "5 apps x 4 titles are 20 groups");
    assert_eq!(mock.count(), 20);
    // the next send of a group, after the window, says how many were held
    clk.advance(700);
    let r = n.send(&Message::new("job 0").app("app0").severity(Severity::Warn), &SendOptions::new());
    assert_eq!(r.disposition.as_str(), "sent");
    let t = titles(&mock);
    let last = t.last().unwrap();
    assert!(last.starts_with("job 0 (x25 since "), "{last}");
}

#[test]
fn state_change_fires_exactly_once_per_transition() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("trans");
    let clk = clock();
    let n = notts(&conf(&mock, ""), &d, clk.clone());
    let send = |s: Severity| {
        clk.advance(6 * 3600);
        n.send(&Message::new("Audit").app("judged").body("body").severity(s), &SendOptions::new().key("judged-audit").state(s)).disposition.as_str()
    };
    let seq = [
        (Severity::Ok, "suppressed"),   // first state is ok: nothing to resolve
        (Severity::Fail, "sent"),       // ok -> fail
        (Severity::Fail, "suppressed"), // periodic job repeats the same failure
        (Severity::Fail, "suppressed"),
        (Severity::Warn, "sent"), // fail -> warn
        (Severity::Warn, "suppressed"),
        (Severity::Ok, "sent"), // resolved
        (Severity::Ok, "suppressed"),
        (Severity::Fail, "sent"),
    ];
    for (s, want) in seq {
        assert_eq!(send(s), want, "{s:?}");
    }
    assert_eq!(mock.count(), 4);
    let t = titles(&mock);
    assert_eq!(t[2], "Resolved: Audit");
    assert!(mock.requests()[2].json()["body"].as_str().unwrap().contains("fail -> ok") || mock.requests()[2].json()["body"].as_str().unwrap().contains("warn -> ok"));
    let h = n.history();
    assert_eq!(h.len(), 9, "every decision is in the history");
    assert!(h.iter().filter(|e| e.decision == "suppressed").all(|e| !e.reason.is_empty()));
}

#[test]
fn a_clock_that_jumps_backwards_does_not_wedge_grouping_or_flood_state() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("back");
    let clk = clock();
    let n = notts(&conf(&mock, "set group=10m\nflood max=5 cooldown=10m"), &d, clk.clone());
    let m = Message::new("t").app("a");
    assert_eq!(n.send(&m, &SendOptions::new()).disposition.as_str(), "sent");
    clk.set_unix(1_795_600_000 - 7200); // two hours back
    // the old group window is in the "future": the message must go out again, not be grouped for 2 h
    assert_eq!(n.send(&m, &SendOptions::new()).disposition.as_str(), "sent");
    // a flood cool-down started "in the future" ends within one cool-down of now
    for i in 0..10 {
        n.send(&Message::new(format!("x{i}")).app("b"), &SendOptions::new());
    }
    clk.set_unix(1_795_600_000 - 20_000);
    let r = n.send(&Message::new("during").app("b"), &SendOptions::new());
    assert_eq!(r.disposition.as_str(), "suppressed", "the cool-down is not cut short by the jump");
    clk.advance(601);
    let r = n.send(&Message::new("after").app("b"), &SendOptions::new());
    assert_eq!(r.disposition.as_str(), "sent", "{r:?}");
}

#[test]
fn a_damaged_state_file_is_unknown_state_and_the_message_is_sent_once() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("damaged");
    let clk = clock();
    let n = notts(&conf(&mock, ""), &d, clk.clone());
    n.send(&Message::new("seed").app("a"), &SendOptions::new());
    let f = notts::state::Store::new(&d.join("state")).file();
    std::fs::write(&f, b"\x00\xff garbage\nstate\tnot\tvalid\n\xc3\x28\n").unwrap();
    let r = n.send(&Message::new("Audit").app("judged"), &SendOptions::new().key("k").state(Severity::Fail));
    assert_eq!(r.disposition.as_str(), "sent", "unknown state: send once. {r:?}");
    let r = n.send(&Message::new("Audit").app("judged"), &SendOptions::new().key("k").state(Severity::Fail));
    assert_eq!(r.disposition.as_str(), "suppressed", "and the repaired state works again");
    assert!(d.join("state").join("flood.state.damaged").exists(), "the damaged file is kept aside");
    // a state file that is a directory or unreadable must not crash either
    let d2 = tmpdir("damaged2");
    std::fs::create_dir_all(d2.join("state").join("flood.state")).unwrap();
    let n2 = notts(&conf(&mock, ""), &d2, clock());
    let r = n2.send(&Message::new("x"), &SendOptions::new());
    assert!(r.exit_code == 0 || r.exit_code == 1, "{r:?}");
}

#[test]
fn digest_collects_info_and_warn_into_one_message_worst_first() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("digest");
    let clk = clock();
    let n = notts(&conf(&mock, "digest every=30m severities=info,warn lines=6"), &d, clk.clone());
    for i in 0..3 {
        let r = n.send(&Message::new("disk").app("judged").severity(Severity::Warn), &SendOptions::new().no_group(true));
        assert_eq!(r.disposition.as_str(), "held", "{i}");
    }
    for i in 0..9 {
        n.send(&Message::new(format!("scan {i}")).app("smart-optimizer").severity(Severity::Info), &SendOptions::new());
    }
    n.send(&Message::new("tick").app("finedesktop").severity(Severity::Info), &SendOptions::new());
    assert_eq!(mock.count(), 0);
    assert!(n.digest(false, false).is_none(), "not due yet");
    clk.advance(31 * 60);
    let r = n.digest(false, false).expect("due");
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(mock.count(), 1, "one message for 13");
    let j = mock.requests()[0].json();
    assert_eq!(j["title"], "Digest: 13 message(s) from 3 app(s)");
    assert_eq!(j["severity"], "warn", "the worst severity");
    let body = j["body"].as_str().unwrap();
    assert!(body.starts_with("judged: 3 message(s), worst warn"), "worst app first: {body}");
    assert!(body.contains("and ") && body.contains(" more ("), "visible truncation marker: {body}");
    assert!(body.lines().count() <= 7);
    // an empty digest is never sent
    clk.advance(3600);
    assert!(n.digest(false, false).is_none());
    assert!(n.digest(true, false).is_none(), "even when forced");
    assert_eq!(mock.count(), 1);
}

#[test]
fn fail_is_never_held_by_the_digest() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("failnever");
    let n = notts(&conf(&mock, "digest every=30m severities=info,warn"), &d, clock());
    let bad = notts::Config::parse("channel desktop\ndigest every=30m severities=info,fail\n").unwrap_err();
    assert!(bad.msg.contains("fail is never held"), "{bad}");
    let r = n.send(&Message::new("disk dead").app("judged").severity(Severity::Fail), &SendOptions::new());
    assert_eq!(r.disposition.as_str(), "sent", "{r:?}");
    assert_eq!(mock.count(), 1);
}

#[test]
fn digest_at_fixed_times_works_across_midnight() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("midnight");
    let clk = clock();
    clk.set_local_minutes(23 * 60 + 45);
    let n = notts(&conf(&mock, "digest at=23:50,00:10 severities=info,warn"), &d, clk.clone());
    n.send(&Message::new("a").app("x").severity(Severity::Info), &SendOptions::new());
    assert!(n.digest(false, false).is_none(), "23:45 is before 23:50");
    clk.advance(10 * 60);
    clk.set_local_minutes(23 * 60 + 55);
    // the first send after the slot flushes the digest of what was waiting ("a"); "b" waits for the next one
    n.send(&Message::new("b").app("x").severity(Severity::Info), &SendOptions::new());
    assert_eq!(mock.count(), 1, "23:50 passed: one digest");
    assert!(titles(&mock)[0].starts_with("Digest: 1 message(s)"));
    // after midnight: the 00:10 slot has the next one
    clk.advance(10 * 60);
    clk.set_local_minutes(5);
    n.send(&Message::new("c").app("x").severity(Severity::Info), &SendOptions::new());
    assert_eq!(mock.count(), 1, "00:05 is before 00:10");
    clk.advance(10 * 60);
    clk.set_local_minutes(15);
    assert!(n.digest(false, false).is_some(), "00:10 passed, across midnight");
    assert_eq!(mock.count(), 2);
    assert!(titles(&mock)[1].starts_with("Digest: 2 message(s)"), "b and c");
    assert!(n.digest(false, false).is_none(), "nothing pending any more");
}

#[test]
fn a_flooding_app_is_switched_to_the_digest_with_one_notice_and_others_are_unaffected() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("flood");
    let clk = clock();
    let n = notts(&conf(&mock, "digest every=1h severities=info\nflood max=10 cooldown=10m"), &d, clk.clone());
    let mut sent = 0;
    for i in 0..100 {
        let r = n.send(&Message::new(format!("noise {i}")).app("noisy").severity(Severity::Warn), &SendOptions::new());
        if r.disposition.as_str() == "sent" {
            sent += 1;
        }
    }
    assert_eq!(sent, 10, "the first 10 pass, the 11th trips the guard");
    let notices: Vec<_> = titles(&mock).into_iter().filter(|t| t.contains("is flooding")).collect();
    assert_eq!(notices, vec!["app noisy is flooding".to_string()], "exactly one notice");
    // another app is not pushed out
    let r = n.send(&Message::new("calm").app("quiet").severity(Severity::Warn), &SendOptions::new());
    assert_eq!(r.disposition.as_str(), "sent");
    // fail from the flooding app is not held
    let r = n.send(&Message::new("real problem").app("noisy").severity(Severity::Fail), &SendOptions::new());
    assert_eq!(r.disposition.as_str(), "sent", "{r:?}");
    // nothing silently lost: held messages are in the history with a reason, and in the digest
    let held = n.history().into_iter().filter(|e| e.decision == "held").count();
    assert_eq!(held, 90);
    let dg = n.digest(true, false).expect("digest");
    assert_eq!(dg.exit_code, 0);
    let last = mock.requests().pop().unwrap().json();
    assert!(last["body"].as_str().unwrap().contains("noisy: 90 message(s)"), "{}", last["body"]);
    // after the cool-down it is normal again
    clk.advance(11 * 60);
    let r = n.send(&Message::new("later").app("noisy").severity(Severity::Warn), &SendOptions::new());
    assert_eq!(r.disposition.as_str(), "sent", "{r:?}");
}

#[test]
fn flood_without_a_digest_drops_into_the_log_only_with_a_reason() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("floodnodigest");
    let n = notts(&conf(&mock, "flood max=3 cooldown=10m"), &d, clock());
    for i in 0..10 {
        n.send(&Message::new(format!("m{i}")).app("noisy"), &SendOptions::new());
    }
    let h = n.history();
    let sup: Vec<_> = h.iter().filter(|e| e.decision == "suppressed").collect();
    assert_eq!(sup.len(), 7, "the 4th message trips the guard and is the first held");
    assert!(sup.iter().all(|e| e.reason.contains("flood cooldown")), "{sup:?}");
}

#[test]
fn per_channel_policy_phone_only_fail_and_resolved_chat_also_warn_mail_only_the_digest() {
    let phone = Mock::start(vec![]);
    let chat = Mock::start(vec![]);
    let mail = Mock::start(vec![]);
    let d = tmpdir("policy");
    let c = format!(
        "set timeout=2 retries=0\nchannel phone type=webhook url={}/p\nchannel chat type=webhook url={}/c\nchannel mail type=webhook url={}/m\n\
         rule min=fail to=phone,chat\nrule min=warn to=chat\ndigest every=1h severities=info to=mail\n",
        phone.url(),
        chat.url(),
        mail.url()
    );
    let clk = clock();
    let n = notts(&c, &d, clk.clone());
    let to = |m: &Mock| m.count();
    n.send(&Message::new("w").severity(Severity::Warn), &SendOptions::new());
    assert_eq!((to(&phone), to(&chat), to(&mail)), (0, 1, 0), "warn: chat only");
    n.send(&Message::new("f").severity(Severity::Fail), &SendOptions::new());
    assert_eq!((to(&phone), to(&chat)), (1, 2), "fail: phone and chat");
    n.send(&Message::new("i").severity(Severity::Info), &SendOptions::new());
    assert_eq!((to(&phone), to(&chat), to(&mail)), (1, 2, 0), "info: no channel selects it by rule, held for the digest");
    // fail -> ok is "resolved" and reaches the phone too
    let o = SendOptions::new().key("svc");
    n.send(&Message::new("svc").severity(Severity::Fail), &o.clone().state(Severity::Fail));
    n.send(&Message::new("svc").severity(Severity::Ok), &o.state(Severity::Ok));
    assert_eq!(to(&phone), 3, "the failure and its resolution");
    assert!(titles(&phone).last().unwrap().starts_with("Resolved:"));
    clk.advance(3601);
    assert!(n.digest(false, false).is_some());
    assert_eq!(to(&mail), 1, "the digest goes to mail");
}

#[test]
fn quiet_hours_hold_everything_except_fail_and_release_on_flush() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("quiet");
    let clk = clock();
    clk.set_local_minutes(23 * 60);
    let n = notts(&conf(&mock, "quiet 22:00-07:00 except=fail"), &d, clk.clone());
    let r = n.send(&Message::new("noise").severity(Severity::Warn), &SendOptions::new());
    assert_eq!((r.disposition.as_str(), r.exit_code), ("queued", 3), "{r:?}");
    assert_eq!(mock.count(), 0);
    let r = n.send(&Message::new("down").severity(Severity::Fail), &SendOptions::new());
    assert_eq!((r.disposition.as_str(), r.exit_code), ("sent", 0));
    assert_eq!(mock.count(), 1);
    // still night: flush keeps it
    let f = n.flush();
    assert_eq!((f.delivered, f.remaining), (0, 1), "{f:?}");
    clk.set_local_minutes(7 * 60 + 1);
    let f = n.flush();
    assert_eq!((f.delivered, f.remaining), (1, 0), "{f:?}");
    assert_eq!(mock.count(), 2);
}

#[test]
fn two_processes_share_one_state_directory_and_exactly_one_sends() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("procs");
    let cfg = d.join("notts.conf");
    std::fs::write(&cfg, conf(&mock, "set group=10m")).unwrap();
    let bin = env!("CARGO_BIN_EXE_notts");
    let mut kids = Vec::new();
    for _ in 0..8 {
        kids.push(
            std::process::Command::new(bin)
                .args(["--config", cfg.to_str().unwrap(), "--state-dir", d.join("state").to_str().unwrap(), "send", "--app", "judged", "--title", "same", "--body", "x", "--json"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let outs: Vec<_> = kids.into_iter().map(|k| k.wait_with_output().unwrap()).collect();
    assert!(outs.iter().all(|o| o.status.code() == Some(0)), "{:?}", outs.iter().map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).collect::<Vec<_>>());
    assert_eq!(mock.count(), 1, "the lock serializes the decision: one send, seven grouped");
    let grouped = outs.iter().filter(|o| String::from_utf8_lossy(&o.stdout).contains("\"disposition\":\"grouped\"")).count();
    assert_eq!(grouped, 7);
    let (st, torn) = (notts::state::Store::new(&d.join("state")).load(), 0);
    assert_eq!((st.damaged, torn), (0, 0), "the state file is intact");
}

// ---- outbox ------------------------------------------------------------------------------------

fn entry(id: &str, ch: &str) -> Entry {
    Entry { msg: Message::new("t").body("zażółć\ttab\nnewline").with_id(id), channel: ch.into(), created: 100, reason: "down".into() }
}

#[test]
fn outbox_survives_a_kill_between_write_and_rename() {
    let d = tmpdir("obx");
    let ob = Outbox::new(&d);
    ob.append(&[entry("a", "x"), entry("b", "x"), entry("c", "y")]).unwrap();
    let before = std::fs::read(ob.file()).unwrap();
    // the process dies after the temp file is complete, before the rename
    ob.remove_with(&[("a".into(), "x".into())], true).unwrap();
    assert_eq!(std::fs::read(ob.file()).unwrap(), before, "the old complete file is untouched");
    assert_eq!(ob.load(), (vec![entry("a", "x"), entry("b", "x"), entry("c", "y")], 0));
    // and a normal removal replaces it whole
    ob.remove(&[("a".into(), "x".into())]).unwrap();
    assert_eq!(ob.count(), 2);
}

#[test]
fn outbox_torn_append_is_counted_and_the_next_append_starts_a_fresh_line() {
    let d = tmpdir("obxtorn");
    let ob = Outbox::new(&d);
    ob.append(&[entry("a", "x")]).unwrap();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(ob.file()).unwrap();
        f.write_all(b"v2\tb\tx\t100\tinfo\t\t\tt\tbod").unwrap(); // killed mid-write, no newline
    }
    let (v, torn) = ob.load();
    assert_eq!((v.len(), torn), (1, 1));
    ob.append(&[entry("c", "x")]).unwrap();
    let (v, torn) = ob.load();
    assert_eq!((v.iter().map(|e| e.msg.id.as_str()).collect::<Vec<_>>(), torn), (vec!["a", "c"], 1));
}

#[test]
fn outbox_entries_round_trip_awkward_text_and_old_v1_lines_load() {
    let d = tmpdir("obxrt");
    let ob = Outbox::new(&d);
    let e = entry("id\twith tab", "ch");
    ob.append(&[e.clone()]).unwrap();
    assert_eq!(ob.load().0, vec![e]);
    // a v1 line (no attachment column) from an older build
    std::fs::write(ob.file(), "v1\tid1\tch\t5\tfail\tapp\ttag\ttitle\tbody\t\twhy\tend\n").unwrap();
    let (v, torn) = ob.load();
    assert_eq!((v.len(), torn), (1, 0));
    assert_eq!((v[0].msg.title.as_str(), v[0].msg.severity, v[0].channel.as_str()), ("title", Severity::Fail, "ch"));
}

#[test]
fn a_flush_never_sends_twice_and_unreachable_channels_stay_queued() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let d = tmpdir("flush");
    let clk = clock();
    let down = format!("set timeout=1 retries=0\nchannel hook type=webhook url=http://127.0.0.1:{port}/in\n");
    let n = notts(&down, &d, clk.clone());
    let r = n.send(&Message::new("t").severity(Severity::Fail), &SendOptions::new());
    assert_eq!((r.exit_code, r.results[0].status), (3, Status::Queued));
    let f = n.flush();
    assert_eq!((f.delivered, f.remaining, f.buried), (0, 1, 0));
    let mock = Mock::start(vec![]);
    let up = notts(&conf(&mock, ""), &d, clk.clone());
    let f = up.flush();
    assert_eq!((f.delivered, f.remaining), (1, 0));
    assert_eq!(up.flush().delivered, 0);
    assert_eq!(mock.count(), 1);
    // entries older than max_age go to outbox.dead, not nowhere
    let n = notts(&down, &d, clk.clone());
    n.send(&Message::new("old").severity(Severity::Fail), &SendOptions::new());
    clk.advance(8 * 24 * 3600);
    let f = n.flush();
    assert_eq!((f.buried, f.remaining), (1, 0), "{f:?}");
    assert!(d.join("state").join("outbox.dead").exists());
}
