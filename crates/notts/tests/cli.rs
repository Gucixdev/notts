//! The CLI contract, exercised through the real binary: flags, exit codes, JSON shape, stdin, files.

mod common;
use common::*;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run(dir: &Path, conf: &str, args: &[&str], stdin: Option<&[u8]>) -> Output {
    let cfg = dir.join("notts.conf");
    std::fs::write(&cfg, conf).unwrap();
    run_with(dir, &cfg, args, stdin)
}

fn run_with(dir: &Path, cfg: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_notts"));
    c.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env("XDG_STATE_HOME", dir.join("xdg-state"))
        .env("TMPDIR", std::env::temp_dir())
        .arg("--config")
        .arg(cfg)
        .arg("--state-dir")
        .arg(dir.join("state"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = c.spawn().unwrap();
    if let Some(b) = stdin {
        let mut si = child.stdin.take().unwrap();
        si.write_all(b).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn conf(mock: &Mock) -> String {
    format!("set timeout=2 retries=0 backoff=10\nchannel hook type=webhook url={}/in\n", mock.url())
}

#[test]
fn version_is_printed_with_the_contract_number() {
    let d = tmpdir("ver");
    let o = run(&d, "", &["--version"], None);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(out(&o).trim(), format!("notts {} (contract {})", notts::VERSION, notts::CONTRACT));
}

#[test]
fn golden_json_pins_the_contract() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("golden");
    let o = run(&d, &conf(&mock), &["send", "--app", "judged", "--severity", "warn", "--title", "Disk almost full", "--body", "/data 97%", "--json"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let want = format!(
        r#"{{"notts_version":"{}","contract":1,"exit_code":0,"disposition":"sent","reason":"sent; hook=sent","key":"Disk almost full","results":[{{"name":"hook","status":"sent","detail":"delivered on attempt 1","delivered_as":"message","parts":1}}]}}"#,
        notts::VERSION
    );
    assert_eq!(out(&o).trim(), want);
    // every other disposition and status name is part of the contract too
    let grouped = run(&d, &conf(&mock), &["send", "--app", "judged", "--title", "Disk almost full", "--json"], None);
    let j: serde_json::Value = serde_json::from_str(out(&grouped).trim()).unwrap();
    assert_eq!((j["disposition"].as_str(), j["results"][0]["status"].as_str(), j["exit_code"].as_i64()), (Some("grouped"), Some("deduped"), Some(0)));
    let dry = run(&d, &conf(&mock), &["send", "--title", "x", "--dry-run", "--json"], None);
    let j: serde_json::Value = serde_json::from_str(out(&dry).trim()).unwrap();
    assert_eq!(j["results"][0]["status"], "skipped");
    assert_eq!(mock.count(), 1, "dry-run and grouped sent nothing");
    let keys: Vec<&str> = j.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["contract", "disposition", "exit_code", "key", "notts_version", "reason", "results"]);
}

#[test]
fn exit_codes_0_1_2_3() {
    let ok = Mock::start(vec![]);
    let d = tmpdir("exit");
    assert_eq!(run(&d, &conf(&ok), &["send", "--title", "a"], None).status.code(), Some(0));
    // 1: the only channel refuses for good
    let bad = Mock::start(vec![status(401)]);
    let o = run(&d, &conf(&bad), &["send", "--title", "b"], None);
    assert_eq!(o.status.code(), Some(1));
    // 1: a channel whose secret is missing is not configured, never sent
    let nc = run(&d, "channel tg type=telegram token_file=/nonexistent/secret chat=1\n", &["send", "--title", "c", "--json"], None);
    assert_eq!(nc.status.code(), Some(1));
    assert!(out(&nc).contains("\"status\":\"not-configured\""), "{}", out(&nc));
    // 3: unreachable, queued
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let q = run(&d, &format!("set timeout=1 retries=0\nchannel h type=webhook url=http://127.0.0.1:{port}/x\n"), &["send", "--title", "d"], None);
    assert_eq!(q.status.code(), Some(3));
    // 2: usage and config
    assert_eq!(run(&d, &conf(&ok), &["send"], None).status.code(), Some(2));
    assert_eq!(run(&d, &conf(&ok), &["send", "--title", "x", "--severity", "loud"], None).status.code(), Some(2));
    assert_eq!(run(&d, &conf(&ok), &["send", "--title", "x", "--nope"], None).status.code(), Some(2));
    assert_eq!(run(&d, &conf(&ok), &["frobnicate"], None).status.code(), Some(2));
    let bad_conf = run(&d, "channel a type=webhook url=http://x\nbogus thing\n", &["send", "--title", "x"], None);
    assert_eq!(bad_conf.status.code(), Some(2));
    assert!(err(&bad_conf).contains("line 2"), "{}", err(&bad_conf));
    let missing = run_with(&d, &d.join("does-not-exist.conf"), &["send", "--title", "x"], None);
    assert_eq!(missing.status.code(), Some(2));
    // required=yes: a delivery to an optional channel alone is not enough
    let req = format!("set timeout=2 retries=0\nchannel must type=webhook url={}/m required=yes\nchannel extra type=webhook url={}/e\n", bad.url(), ok.url());
    bad.push(status(401));
    let o = run(&d, &req, &["send", "--title", "r", "--key", "r1"], None);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
}

#[test]
fn body_from_flag_stdin_and_file_and_stdin_dash() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("body");
    let c = conf(&mock);
    run(&d, &c, &["send", "--title", "one", "--body", "from flag"], None);
    run(&d, &c, &["send", "--title", "two"], Some("zażółć z stdin\n".as_bytes()));
    let f = d.join("summary.txt");
    std::fs::write(&f, "from file").unwrap();
    run(&d, &c, &["send", "--title", "three", "--body-file", f.to_str().unwrap()], None);
    run(&d, &c, &["send", "--title", "four", "--body-file", "-"], Some(b"dash"));
    let r = mock.requests();
    let bodies: Vec<String> = r.iter().map(|q| q.json()["body"].as_str().unwrap().to_string()).collect();
    assert_eq!(bodies, vec!["from flag", "zażółć z stdin", "from file", "dash"]);
    // not both, and a missing file is a usage error
    assert_eq!(run(&d, &c, &["send", "--title", "x", "--body", "a", "--body-file", f.to_str().unwrap()], None).status.code(), Some(2));
    let o = run(&d, &c, &["send", "--title", "x", "--body-file", "/nonexistent/f"], None);
    assert_eq!(o.status.code(), Some(2));
    // invalid UTF-8 on stdin is replaced, not rejected
    let o = run(&d, &c, &["send", "--title", "bin"], Some(&[b'o', b'k', 0xff, 0xfe]));
    assert_eq!(o.status.code(), Some(0));
    // more than 4 MiB on stdin is refused, not silently cut
    let huge = vec![b'x'; 4 * 1024 * 1024 + 10];
    let o = run(&d, &c, &["send", "--title", "huge"], Some(&huge));
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("larger than"), "{}", err(&o));
}

#[test]
fn attach_flags_pair_names_and_enforce_limits() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("attach");
    let c = conf(&mock);
    let (a, b) = (d.join("raport.md"), d.join("second.txt"));
    std::fs::write(&a, "# report\n").unwrap();
    std::fs::write(&b, "two").unwrap();
    let o = run(
        &d,
        &c,
        &[
            "send",
            "--app",
            "judged",
            "--key",
            "judged-audit",
            "--state",
            "fail",
            "--title",
            "Audit",
            "--body-file",
            "-",
            "--attach",
            a.to_str().unwrap(),
            "--attach-name",
            "audit-2026-11-24.md",
            "--attach",
            b.to_str().unwrap(),
            "--json",
        ],
        Some(b"summary"),
    );
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    let at = mock.requests()[0].json()["attachments"].clone();
    assert_eq!((at[0]["name"].as_str(), at[0]["text"].as_str()), (Some("audit-2026-11-24.md"), Some("# report\n")));
    assert_eq!(at[1]["name"], "second.txt", "an unnamed attachment keeps its file name");
    assert_eq!(run(&d, &c, &["send", "--title", "x", "--attach", "/nonexistent/x"], None).status.code(), Some(2));
    assert_eq!(run(&d, &c, &["send", "--title", "x", "--attach-name", "orphan.md"], None).status.code(), Some(2));
    assert_eq!(run(&d, &c, &["send", "--title", "x", "--attach", d.to_str().unwrap()], None).status.code(), Some(2), "a directory is not a file");
    let many: Vec<String> = (0..4).flat_map(|_| ["--attach".to_string(), a.to_str().unwrap().to_string()]).collect();
    let mut args = vec!["send".to_string(), "--title".into(), "x".into()];
    args.extend(many);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    assert_eq!(run(&d, &c, &refs, None).status.code(), Some(2), "at most 3");
    let big = d.join("big.bin");
    std::fs::write(&big, vec![0u8; notts::MAX_ATTACHMENT + 1]).unwrap();
    let o = run(&d, &c, &["send", "--title", "x", "--attach", big.to_str().unwrap()], None);
    assert_eq!(o.status.code(), Some(2));
    // judged flow: unchanged failing report is not sent again, resolved comes without the file
    let again = run(&d, &c, &["send", "--app", "judged", "--key", "judged-audit", "--state", "fail", "--title", "Audit", "--body", "summary", "--attach", a.to_str().unwrap(), "--json"], None);
    assert!(out(&again).contains("\"disposition\":\"suppressed\"") && out(&again).contains("unchanged"), "{}", out(&again));
    let n = mock.count();
    let res = run(&d, &c, &["send", "--app", "judged", "--key", "judged-audit", "--state", "ok", "--title", "Audit", "--body", "all good", "--attach", a.to_str().unwrap(), "--json"], None);
    assert_eq!(res.status.code(), Some(0));
    assert_eq!(mock.count(), n + 1);
    let last = mock.requests().pop().unwrap().json();
    assert!(last.get("attachments").is_none_or(|a| a.as_array().is_some_and(|v| v.is_empty())), "resolved carries no file: {last}");
    assert!(last["title"].as_str().unwrap().starts_with("Resolved:"));
}

#[test]
fn log_status_flush_and_digest_subcommands() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("sub");
    let c = format!("{}digest every=30m severities=info\n", conf(&mock));
    run(&d, &c, &["send", "--app", "a", "--title", "t1", "--severity", "warn"], None);
    run(&d, &c, &["send", "--app", "a", "--title", "t1", "--severity", "warn"], None);
    run(&d, &c, &["send", "--app", "b", "--title", "quiet one", "--severity", "info"], None);
    let l = run(&d, &c, &["log"], None);
    let text = out(&l);
    assert!(text.contains("sent") && text.contains("grouped") && text.contains("held"), "{text}");
    let only_a = out(&run(&d, &c, &["log", "--app", "a"], None));
    assert!(only_a.contains("t1") && !only_a.contains("quiet one"), "{only_a}");
    assert!(out(&run(&d, &c, &["log", "--since", "2h"], None)).contains("t1"));
    let g = out(&run(&d, &c, &["log", "--grouped"], None));
    assert!(g.contains("t1"), "{g}");
    let st = run(&d, &c, &["status", "--json"], None);
    let j: serde_json::Value = serde_json::from_str(out(&st).trim()).unwrap();
    assert_eq!(j["notts_version"], notts::VERSION);
    assert_eq!(j["channels"][0]["name"], "hook");
    let dry = run(&d, &c, &["digest", "--now", "--dry-run"], None);
    assert_eq!(dry.status.code(), Some(0), "{}", err(&dry));
    assert_eq!(mock.count(), 1, "a dry digest sends nothing");
    let real = run(&d, &c, &["digest", "--now"], None);
    assert_eq!(real.status.code(), Some(0));
    assert_eq!(mock.count(), 2);
    assert!(mock.requests()[1].json()["title"].as_str().unwrap().starts_with("Digest:"));
    let empty = run(&d, &c, &["digest", "--now"], None);
    assert_eq!(empty.status.code(), Some(0));
    assert_eq!(mock.count(), 2, "an empty digest is never sent");
    assert_eq!(run(&d, &c, &["flush"], None).status.code(), Some(0));
}

#[test]
fn hook_turns_the_finedesktop_environment_into_a_message() {
    let mock = Mock::start(vec![]);
    let d = tmpdir("hook");
    let cfg = d.join("notts.conf");
    std::fs::write(&cfg, conf(&mock)).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_notts"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &d)
        .env("FD_TEXT", "Battery low\nplug in the charger")
        .env("FD_APP", "finedesktop")
        .env("FD_URGENCY", "critical")
        .env("FD_TAG", "battery")
        .arg("--config")
        .arg(&cfg)
        .arg("--state-dir")
        .arg(d.join("state"))
        .arg("hook")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let j = mock.requests()[0].json();
    assert_eq!((j["app"].as_str(), j["severity"].as_str()), (Some("finedesktop"), Some("fail")), "{j}");
    assert!(j["title"].as_str().unwrap().contains("Battery low"));
}

#[test]
fn secrets_never_reach_stdout_or_stderr() {
    let mock = Mock::start(vec![status(500), status(500), status(500)]);
    let d = tmpdir("mask");
    let tok = secret_file(&d, "tok", "123456:SUPER-SECRET-TOKEN");
    let c = format!("set timeout=2 retries=1 backoff=10\nchannel tg type=telegram token_file={tok} chat=42 api={}\n", mock.url());
    let o = run(&d, &c, &["send", "--title", "x", "--json"], None);
    let all = format!("{}{}", out(&o), err(&o));
    assert!(!all.contains("SUPER-SECRET") && !all.contains("123456:"), "{all}");
    let o = run(&d, &c, &["status"], None);
    assert!(!format!("{}{}", out(&o), err(&o)).contains("SUPER-SECRET"));
    // a group/world-readable secret file is refused with a clear message, and its content is not echoed
    std::fs::set_permissions(&tok, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let o = run(&d, &c, &["send", "--title", "y", "--json"], None);
    let all = format!("{}{}", out(&o), err(&o));
    assert_eq!(o.status.code(), Some(1));
    assert!(all.contains("not-configured") && all.contains("chmod 600"), "{all}");
    assert!(!all.contains("SUPER-SECRET"));
}
