//! The snippets from the README, run for real against the built binary and a mock server:
//! shelling out from Python, Go and sh, and the unix-socket clients for `notts serve` in Go and Node.

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn bin_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_notts")).parent().unwrap().to_path_buf()
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() || Command::new(tool).arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

struct Setup {
    dir: PathBuf,
    conf: PathBuf,
}

fn setup(mock: &Mock, tag: &str) -> Setup {
    let dir = tmpdir(tag);
    let conf = dir.join("notts.conf");
    std::fs::write(&conf, format!("set timeout=3 retries=0\nchannel hook type=webhook url={}/in\n", mock.url())).unwrap();
    Setup { dir, conf }
}

fn tool_cmd(s: &Setup, prog: &str) -> Command {
    let mut c = Command::new(prog);
    let path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir().display());
    c.env("PATH", path)
        .env("NOTTS_CONFIG", &s.conf)
        .env("XDG_STATE_HOME", s.dir.join("xdg-state"))
        .env("XDG_RUNTIME_DIR", &s.dir)
        .env("GOCACHE", std::env::temp_dir().join("notts-go-cache"))
        .env("GOPATH", std::env::temp_dir().join("notts-go-path"))
        .env("GOFLAGS", "-buildvcs=false")
        .env("GO111MODULE", "off")
        .env_remove("http_proxy")
        .env_remove("HTTP_PROXY");
    c
}

#[test]
fn python_subprocess_snippet() {
    if !have("python3") {
        return;
    }
    let mock = Mock::start(vec![]);
    let s = setup(&mock, "py");
    let o = tool_cmd(&s, "python3").arg("-I").arg(examples().join("send.py")).args(["judged", "warn", "Zażółć gęślą", "body ł"]).output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("sent [('hook', 'sent')]"), "{}", String::from_utf8_lossy(&o.stdout));
    let j = mock.requests()[0].json();
    assert_eq!((j["app"].as_str(), j["severity"].as_str(), j["title"].as_str(), j["body"].as_str()), (Some("judged"), Some("warn"), Some("Zażółć gęślą"), Some("body ł")));
}

#[test]
fn shell_snippet_and_its_exit_code() {
    let mock = Mock::start(vec![status(401)]);
    let s = setup(&mock, "sh");
    let o = tool_cmd(&s, "sh").arg(examples().join("send.sh")).args(["smart-optimizer", "info", "Cache cleaned", "freed 2 GB"]).output().unwrap();
    assert_eq!(o.status.code(), Some(1), "the mock refused with 401: exit 1 reaches the shell. {}", String::from_utf8_lossy(&o.stderr));
    let o = tool_cmd(&s, "sh").arg(examples().join("send.sh")).args(["smart-optimizer", "info", "Second", "x"]).output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(mock.requests()[1].json()["body"], "x");
}

#[test]
fn go_exec_snippet() {
    if !have("go") {
        return;
    }
    let mock = Mock::start(vec![]);
    let s = setup(&mock, "go");
    let o = tool_cmd(&s, "go").arg("run").arg(examples().join("send.go")).args(["finedesktop", "fail", "Battery", "2%"]).output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("\"disposition\":\"sent\""));
    assert_eq!(mock.requests()[0].json()["app"], "finedesktop");
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_serve(s: &Setup) -> (Daemon, PathBuf) {
    let sock = s.dir.join("notts.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_notts"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &s.dir)
        .env("XDG_STATE_HOME", s.dir.join("xdg-state"))
        .arg("--config")
        .arg(&s.conf)
        .arg("--state-dir")
        .arg(s.dir.join("state"))
        .arg("serve")
        .arg("--socket")
        .arg(&sock)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    while !sock.exists() && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sock.exists(), "serve did not come up");
    (Daemon(child), sock)
}

#[test]
fn serve_clients_in_go_and_node_over_the_unix_socket() {
    let mock = Mock::start(vec![]);
    let s = setup(&mock, "serveclients");
    let (_d, sock) = start_serve(&s);
    if have("go") {
        let o = tool_cmd(&s, "go").arg("run").arg(examples().join("serve_client.go")).arg(&sock).args(["fastkit-api", "Queue stuck", "ł"]).output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout).into_owned();
        assert!(out.starts_with("200 ") && out.contains("\"disposition\":\"sent\""), "{out}{}", String::from_utf8_lossy(&o.stderr));
    }
    if have("node") {
        let o = tool_cmd(&s, "node").arg(examples().join("serve_client.mjs")).arg(&sock).args(["fastkit-web", "Build failed", "zażółć"]).output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout).into_owned();
        assert!(out.starts_with("200 ") && out.contains("\"disposition\":\"sent\""), "{out}{}", String::from_utf8_lossy(&o.stderr));
    }
    let reqs = mock.requests();
    assert!(!reqs.is_empty());
    assert!(reqs.iter().all(|r| r.json()["severity"] == "warn"));
    let apps: Vec<String> = reqs.iter().map(|r| r.json()["app"].as_str().unwrap().to_string()).collect();
    assert!(apps.iter().all(|a| a.starts_with("fastkit-")), "{apps:?}");
}

#[test]
fn serve_binary_refuses_non_loopback_and_missing_token() {
    let mock = Mock::start(vec![]);
    let s = setup(&mock, "servebin");
    let tok = secret_file(&s.dir, "tok", "abc");
    for args in [vec!["--listen", "0.0.0.0:7780", "--token-file", tok.as_str()], vec!["--listen", "127.0.0.1:0"]] {
        let o = Command::new(env!("CARGO_BIN_EXE_notts")).arg("--config").arg(&s.conf).arg("--state-dir").arg(s.dir.join("st")).arg("serve").arg("--no-socket").args(&args).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        let e = String::from_utf8_lossy(&o.stderr).into_owned();
        assert!(e.contains("loopback") || e.contains("--token-file"), "{e}");
    }
}
